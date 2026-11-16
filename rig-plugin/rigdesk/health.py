"""A RigDesk PC's health as Rig judges it. The PC takes a sample of raw figures every 10 minutes and keeps a day of them; Rig reads
them after the last it holds, keeps a week per PC, and decides ok, warn, fault or stale here, from thresholds in its settings."""
from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path
from typing import Annotated, Any, Literal, cast

from pydantic import BaseModel, ConfigDict, Field, ValidationError

from core.base import JsonObject, event_log, replace_file
from core.node_data import Shape

SCHEMA = 1
KEPT_MS = 7 * 24 * 3600_000
# The file is rewritten once its oldest sample is this far past the week, not on every sample.
REWRITE_SLACK_MS = 24 * 3600_000
_LOG = event_log(__name__)

type State = Literal["ok", "warn", "fault", "stale"]
STATES: tuple[State, ...] = ("ok", "warn", "fault", "stale")


class _Read(BaseModel):
    """Figures as the PC prints them: read once here; a field a newer PC adds is ignored, a missing one is absent."""
    model_config = ConfigDict(frozen=True, extra="ignore")


class Os(_Read):
    caption: str = ""
    version: str = ""
    build: str = ""


class Cpu(_Read):
    name: str = ""
    load_pct: float | None = None


class Ram(_Read):
    total_mb: int
    free_mb: int


class Volume(_Read):
    name: str
    label: str = ""
    total_mb: int
    free_mb: int

    @property
    def free_pct(self) -> float:
        return 100.0 * self.free_mb / self.total_mb if self.total_mb else 0.0


class Updates(_Read):
    checked_ms: int | None = None
    last_install_ms: int | None = None
    pending: int | None = None
    error: str | None = None


class Defender(_Read):
    realtime_on: bool | None = None
    signature_age_days: int | None = None


class Encryption(_Read):
    volume: str
    protection: str
    status: str = ""


class Disk(_Read):
    name: str
    media: str = ""
    health: str
    wear_pct: int | None = None
    temperature_c: int | None = None


class Service(_Read):
    name: str
    display: str = ""
    delayed: bool = False
    exit_code: int | None = None


class Battery(_Read):
    charge_pct: int | None = None
    status: str = ""


class Figures(_Read):
    schema_: Annotated[int, Field(alias="schema")]
    os: Os
    uptime_s: int
    cpu: Cpu
    ram: Ram
    volumes: tuple[Volume, ...] = ()
    pending_reboot: bool | None = None
    pending_reboot_why: tuple[str, ...] = ()
    updates: Updates | None = None
    defender: Defender | None = None
    bitlocker: tuple[Encryption, ...] = ()
    disks: tuple[Disk, ...] = ()
    stopped_services: tuple[Service, ...] = ()
    system_errors_24h: int | None = None
    battery: Battery | None = None
    cpu_ms: int | None = None
    errors: tuple[str, ...] = ()


class Sample(_Read):
    """One run of the PC's collector: its figures, or why it took none. id rises on the PC across its restarts and reboots."""
    id: int
    at_ms: int
    boot: str = ""
    rigdesk: str = ""
    took_ms: int = 0
    taken: Literal["ok", "failed"]
    figures: Figures | None = None
    why: str | None = None


def samples_of(said: object) -> tuple[list[Sample], list[str]]:
    """GET /samples's answer, read: the samples, and why each one that does not read was skipped."""
    rows: object = cast(dict[str, Any], said).get("samples") if isinstance(said, dict) else None
    read: list[Sample] = []
    skipped: list[str] = []
    for row in cast(list[object], rows) if isinstance(rows, list) else []:
        try:
            sample = Sample.model_validate(row)
        except ValidationError as broken:
            skipped.append(f"sample {cast(dict[str, Any], row).get('id') if isinstance(row, dict) else '?'}: {broken.errors()[0]['msg']}")
            continue
        if sample.figures is not None and sample.figures.schema_ != SCHEMA:
            skipped.append(f"sample {sample.id}: figures schema {sample.figures.schema_}, this plugin reads {SCHEMA}")
            continue
        read.append(sample)
    return read, skipped


class Thresholds(Shape):
    model_config = ConfigDict(defer_build=True, json_schema_extra={"description": "Where Rig judges a RigDesk PC's figures warn or fault; the PC reports figures only."})
    disk_free_warn_pct: Annotated[float, Field(ge=0, le=100, description="A volume with less free space than this, in percent, warns.")] = 15
    disk_free_fault_pct: Annotated[float, Field(ge=0, le=100, description="A volume with less free space than this, in percent, is a fault.")] = 5
    disk_wear_warn_pct: Annotated[int, Field(ge=0, le=100, description="A physical disk worn this far, in percent, warns.")] = 80
    signature_warn_days: Annotated[int, Field(ge=0, description="Defender signatures older than this, in days, warn.")] = 3
    signature_fault_days: Annotated[int, Field(ge=0, description="Defender signatures older than this, in days, are a fault.")] = 7
    system_errors_warn: Annotated[int, Field(ge=0, description="More System-log errors than this in the last 24 h warn.")] = 25
    stale_after_min: Annotated[int, Field(ge=11, description="A PC up with no sample for this many minutes reads stale; it samples every 10.")] = 25


@dataclass(frozen=True)
class Check:
    """One figure out of bounds: what it is, how bad, and the words an agent and the page read."""
    name: str
    state: Literal["warn", "fault", "stale"]
    words: str


@dataclass(frozen=True)
class Health:
    state: State
    failing: tuple[Check, ...]

    @property
    def said(self) -> str:
        return "; ".join(check.words for check in self.failing)


def health(sample: Sample, thresholds: Thresholds, now_ms: int, up: bool) -> Health:
    """The PC's health from its newest sample: stale when it is up and that sample is too old, else the worst of its checks.
    A figure the PC could not read is unknown, never a fault."""
    age_min = (now_ms - sample.at_ms) / 60_000
    if up and age_min > thresholds.stale_after_min:
        return Health("stale", (Check("sample", "stale", f"no health sample for {age_min:.0f} min"),))
    if sample.figures is None:
        return Health("warn", (Check("sample", "warn", f"the health script failed: {sample.why or 'no reason given'}"),))
    figures, failing = sample.figures, list[Check]()
    for volume in (volume for volume in figures.volumes if volume.total_mb > 0):
        free = volume.free_pct
        if free < thresholds.disk_free_fault_pct:
            failing.append(Check(f"disk {volume.name}", "fault", f"{volume.name} {free:.0f}% free (fault under {thresholds.disk_free_fault_pct:g}%)"))
        elif free < thresholds.disk_free_warn_pct:
            failing.append(Check(f"disk {volume.name}", "warn", f"{volume.name} {free:.0f}% free (warn under {thresholds.disk_free_warn_pct:g}%)"))
    for disk in figures.disks:
        if disk.health.lower() == "unhealthy":
            failing.append(Check(f"drive {disk.name}", "fault", f"{disk.name} is unhealthy"))
        elif disk.health.lower() == "warning":
            failing.append(Check(f"drive {disk.name}", "warn", f"{disk.name} reports a warning"))
        elif disk.wear_pct is not None and disk.wear_pct >= thresholds.disk_wear_warn_pct:
            failing.append(Check(f"drive {disk.name}", "warn", f"{disk.name} {disk.wear_pct}% worn"))
    defender = figures.defender
    if defender is not None and defender.realtime_on is False:
        failing.append(Check("defender", "fault", "Defender real-time protection is off"))
    age = None if defender is None else defender.signature_age_days
    if age is not None and age > thresholds.signature_fault_days:
        failing.append(Check("signatures", "fault", f"Defender signatures {age} days old"))
    elif age is not None and age > thresholds.signature_warn_days:
        failing.append(Check("signatures", "warn", f"Defender signatures {age} days old"))
    errors = figures.system_errors_24h
    if errors is not None and errors > thresholds.system_errors_warn:
        failing.append(Check("system log", "warn", f"{errors} System-log errors in 24 h"))
    crashed = [service for service in figures.stopped_services if service.exit_code not in (None, 0, 1077)]
    if crashed:
        failing.append(Check("services", "warn", "stopped with an error: " + ", ".join(service.display or service.name for service in crashed)))
    state: State = "fault" if any(check.state == "fault" for check in failing) else "warn" if failing else "ok"
    return Health(state, tuple(failing))


class History:
    """A week of samples per PC, in memory and in <dir>/<pc>.jsonl: appended as they come, rewritten once a day's worth is past the week.
    The bound is the invalidation."""

    def __init__(self, folder: Path) -> None:
        self._dir = folder
        self._kept: dict[str, list[Sample]] = {}

    def _file(self, pc: str) -> Path:
        return self._dir / f"{pc}.jsonl"

    def of(self, pc: str) -> list[Sample]:
        if pc not in self._kept:
            self._kept[pc] = self._read(pc)
        return self._kept[pc]

    def _read(self, pc: str) -> list[Sample]:
        try:
            lines = self._file(pc).read_text().splitlines()
        except FileNotFoundError:
            return []
        kept: list[Sample] = []
        for line in lines:
            try:
                kept.append(Sample.model_validate_json(line))
            except ValidationError:
                _LOG.warning("rigdesk_sample_unread", f"RigDesk {pc}: a kept health sample does not read and was dropped", pc=pc)
        return kept

    def newest(self, pc: str) -> Sample | None:
        kept = self.of(pc)
        return kept[-1] if kept else None

    def add(self, pc: str, samples: list[Sample], now_ms: int) -> list[Sample]:
        """Keeps those newer than the newest held, by the PC's clock, so a PC whose ids began again is still read; answers those kept.
        Blocks on the disk."""
        kept = self.of(pc)
        newest = kept[-1].at_ms if kept else 0
        fresh = sorted((sample for sample in samples if sample.at_ms > newest), key=lambda sample: sample.at_ms)
        if not fresh:
            return []
        kept += fresh
        cutoff = now_ms - KEPT_MS
        self._dir.mkdir(mode=0o700, parents=True, exist_ok=True)
        if kept[0].at_ms < cutoff - REWRITE_SLACK_MS:
            kept[:] = [sample for sample in kept if sample.at_ms >= cutoff]
            replace_file(self._file(pc), "".join(sample.model_dump_json(by_alias=True) + "\n" for sample in kept).encode(), new_mode=0o600)
        else:
            with open(self._file(pc), "a", encoding="utf-8") as file:
                file.writelines(sample.model_dump_json(by_alias=True) + "\n" for sample in fresh)
        return fresh

    def forget(self, pc: str) -> None:
        self._kept.pop(pc, None)


def series(samples: list[Sample]) -> list[JsonObject]:
    """Each sample's free space per volume, in percent, oldest first, for the page's disk-free line."""
    return [{"at_ms": sample.at_ms, "free_pct": {volume.name: round(volume.free_pct, 1) for volume in sample.figures.volumes}}
            for sample in samples if sample.figures is not None]


def figures_json(sample: Sample) -> JsonObject:
    return cast(JsonObject, json.loads(sample.model_dump_json(by_alias=True)))
