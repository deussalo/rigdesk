import { useEffect, useState, type ReactNode } from 'react'
import { ALL_SPACE, get_agent_settings, get_graph_cards, get_graphs, patch_agent_settings } from '@rig/contract'
import { Action, agoOf, Board, BoardBar, BoardFacts, BoardForm, BoardNew, BoardPart, BoardProblems, BoardRow, BoardRows, bytesOf, contribution, durationOf, editSettings, Field, Headed, href, lastOf, Link,
  Mark, MenuRow, objectOf, Quiet, REREAD_MS, Refusal, store as core, tell, Text, Toggle, told, useAsked, useNow, useRememberedFold, withPath, type BoardColumns, type Fact, type Ink,
  type Refused } from '@rig/host'
import css from './rigdesk.module.css'

const routes = contribution('rigdesk')
const EVERY_MS = Math.min(REREAD_MS, 5_000)

type Owner = { readonly id: string | null; readonly title: string | null; readonly graph: string | null }
type Grant = { readonly grantee: string; readonly label: string; readonly graph: string | null; readonly agent_id: string | null }
type Shot = { readonly mime: string; readonly data: string; readonly taken_ms: number }
type McpServer = { readonly name: string; readonly key: string; readonly says: string; readonly tools: readonly string[]; readonly since_ms: number }
/** The PC's own MCP servers: Rig's switch, the PC's as it last said (null while no agent holds it), why none is offered, and those open for the holder. */
type Mcp = { readonly allowed: boolean; readonly on: boolean | null; readonly why: string | null; readonly servers: readonly McpServer[] }
type Level = 'user' | 'system'
type ShellSwitch = { readonly open: boolean; readonly code: string | null; readonly words: string | null }
type ShellGrant = Grant & { readonly level: Level }
/** A RigDesk PC's agent shell: the PC's own switch as it last said (null: its RigDesk has none), and who may run commands there with no session. */
type PcShell = { readonly switch: ShellSwitch | null; readonly granted: readonly ShellGrant[] }
type Verdict = 'ok' | 'warn' | 'fault' | 'stale'
type Check = { readonly name: string; readonly state: Exclude<Verdict, 'ok'>; readonly words: string }
type Volume = { readonly name: string; readonly free_pct: number; readonly free_mb: number; readonly total_mb: number }
type Headline = {
  readonly volumes: readonly Volume[]
  readonly ram_used_pct: number | null
  readonly cpu_pct: number | null
  readonly build: string
  readonly uptime_s: number
  readonly pending_reboot: boolean | null
}
/** Rig's verdict on the PC's newest sample (null while it holds none), the checks that fail, and the headline figures. */
type PcHealth = {
  readonly state: Verdict | null
  readonly failing: readonly Check[]
  readonly why: string | null
  readonly sample_ms: number | null
  readonly rigdesk: string | null
  readonly figures: Headline | null
}
type PcRow = {
  readonly name: string
  readonly client: string
  readonly url: string
  readonly stock: boolean
  readonly state: 'connecting' | 'up' | 'down'
  readonly why: string | null
  readonly down_since_ms: number | null
  readonly holder: 'human' | 'agent' | 'blocked' | null
  readonly owner: Owner | null
  readonly owned_here: boolean
  readonly assigned: readonly Grant[]
  readonly mcp?: Mcp | undefined
  readonly shell?: PcShell | null | undefined
  readonly health?: PcHealth | null | undefined
}
type Listener = {
  readonly role: 'hbbs' | 'hbbr'
  readonly address: string
  readonly state: 'unchecked' | 'up' | 'down'
  readonly why: string | null
  readonly since_ms: number | null
  readonly checked_ms: number | null
}
type IdServer = { readonly host: string; readonly listeners: readonly Listener[]; readonly install: string | null; readonly install_why: string | null }
type PcsView = { readonly pcs: readonly PcRow[]; readonly id_server: IdServer | null; readonly for_fleet: boolean }
type PcAdded = { readonly name: string; readonly entry: Readonly<Record<string, unknown>> }

const INK: Readonly<Record<PcRow['state'], Ink>> = { up: 'live', connecting: 'hold', down: 'fault' }
const VERDICT_INK: Readonly<Record<Verdict, Ink>> = { ok: 'idle', warn: 'hold', fault: 'fault', stale: 'hold' }
const LISTENER_INK: Readonly<Record<Listener['state'], Ink>> = { up: 'live', unchecked: 'hold', down: 'fault' }
const ROLE: Readonly<Record<Listener['role'], string>> = { hbbs: 'ID server', hbbr: 'relay' }
const NAME = /^[a-z0-9][a-z0-9-]{0,62}$/
const RUSTDESK_ID = /^[0-9]{6,32}$/
const PASSWORD = /^[\x21-\x7e]{6,128}$/
const GRAPHS = { route: get_graphs, params: {} } as const
const AGENTS_OFFERED = 100
const WHOLE_GRAPH = ''
const DESK_SERVER = 'desk'
const textOf = (value: unknown): string => (typeof value === 'string' ? value : '')
const HOLDER: Readonly<Record<NonNullable<PcRow['holder']>, string>> = { human: 'human holds input', agent: 'agent holds input', blocked: 'agent blocked' }

/** A PC's row: its client and kind under its name, who holds input or what fails under its state, who is on it or why it is down, its health figures. */
const COLUMNS: BoardColumns = { who: '30ch', how: '20ch', figures: '30ch', end: '8ch' }
/** What needs the operator sorts first, what is broken last. */
const ORDER: Readonly<Record<string, number>> = { blocked: 0, held: 1, fault: 2, warn: 3, stale: 4, up: 5, connecting: 6, down: 7 }
/** An up PC nobody holds reads as its health when that is not ok. */
const standingOf = (pc: PcRow): string => {
  if (pc.holder === 'blocked') return 'blocked'
  if (pc.state === 'up' && pc.owner !== null) return 'held'
  const verdict = pc.health?.state ?? null
  return pc.state === 'up' && verdict !== null && verdict !== 'ok' ? verdict : pc.state
}
const inkOf = (standing: string, pc: PcRow): Ink => (standing === 'blocked' ? 'fault' : standing in VERDICT_INK ? VERDICT_INK[standing as Verdict] : INK[pc.state])
const rankOf = (pc: PcRow): number => ORDER[standingOf(pc)] ?? 5
const byStanding = (one: PcRow, other: PcRow): number => {
  const apart = rankOf(one) - rankOf(other)
  return apart === 0 ? one.name.localeCompare(other.name) : apart
}
const copied = (what: string, text: string): void => {
  void navigator.clipboard.writeText(text).then(() => { tell({ key: `rigdesk:${what}`, tone: 'info', source: 'RigDesk', message: `${what} copied` }) },
    () => { tell({ key: `rigdesk:${what}`, tone: 'fault', source: 'RigDesk', message: `${what} not copied: the browser refused the clipboard` }) })
}

export function RigdeskTab(): ReactNode {
  return <MenuRow plugin="rigdesk" path="all" label="RigDesk" icon="bento" />
}

/** One of the plugin's GET routes, read now and every everyMs while the part showing it is open. */
function useRead<T>(path: string, everyMs: number = EVERY_MS): T | Refused | null {
  const [read, setRead] = useState<T | Refused | null>(null)
  useEffect(() => {
    let live = true
    const once = async (): Promise<void> => {
      const result = await routes('GET', path)
      if (live) setRead(result.ok ? result.value as T : result.refused)
    }
    void once()
    const timer = setInterval(() => { void once() }, everyMs)
    return () => { live = false; clearInterval(timer) }
  }, [path, everyMs])
  return read
}

const refusedOf = <T,>(read: T | Refused | null): Refused | null => (read !== null && typeof read === 'object' && 'rendered' in read ? read as Refused : null)
const valueOf = <T,>(read: T | Refused | null): T | null => (read === null || refusedOf(read) !== null ? null : read as T)

function OwnerOf({ owner }: { readonly owner: Owner }): ReactNode {
  const title = owner.title ?? owner.id ?? 'an agent'
  return owner.graph !== null && owner.id !== null ? <Link href={href('agent', { graph: owner.graph, agent: owner.id })}>{title}</Link> : <>{title}</>
}

/** A standing grant, kept by rigdesk: the agent acts on this stock PC with no share link, across restarts, until unassigned. */
const assigning = (pc: string, grantee: string, granted: boolean) => async (): Promise<Refused | null> => {
  const done = await routes('POST', 'assign', { pc, grantee, granted })
  return done.ok ? null : told(granted ? 'assign failed' : 'unassign failed', done.refused)
}

/** Who holds a standing grant to a stock PC, each a line with its unassign. */
function Grants({ pc }: { readonly pc: PcRow }): ReactNode {
  return pc.assigned.map(each => (
    <BoardBar key={each.grantee} apart={<Action label="unassign" tone="outlined" pending="unassigning…" onInvoke={assigning(pc.name, each.grantee, false)} />}>
      {each.agent_id !== null && each.graph !== null ? <Link href={href('agent', { graph: each.graph, agent: each.agent_id })}>{each.label}</Link> : <span>{each.label}</span>}
    </BoardBar>
  ))
}

/** Opens the desk server on one agent, as the operator, when desk/* is not open to every agent: an agent never raises its own reach. */
const openDesk = async (graph: string, agent: string): Promise<Refused | null> => {
  const read = await core.post({ route: get_agent_settings, params: { name: graph, agent_id: agent } })
  if (!read.ok) return told('assigned, but desk not opened for it', read.refused)
  const servers = objectOf(read.value.config['mcp_servers']) ?? null
  if (servers?.[DESK_SERVER] === 'server') return null
  const saved = await core.post({ route: patch_agent_settings, params: { name: graph, agent_id: agent },
    body: { settings: { mcp_servers: { ...servers, [DESK_SERVER]: 'server' } }, expected: { mcp_servers: servers }, request_id: crypto.randomUUID() } })
  return saved.ok ? null : told('assigned, but desk not opened for it', saved.refused)
}

/** Who a grant goes to: one agent, or every agent in a graph, picked by graph and then by title. */
type Grantee = { readonly graph: string | null; readonly agent: string | null; readonly grant: string; readonly fields: (about: string) => ReactNode; readonly none: boolean }

function useGrantee(): Grantee {
  const graphs = (lastOf(useAsked(GRAPHS)) ?? []).flatMap(row => (row.type === 'graph' && !row.flow ? [row.name] : []))
  const [graph, setGraph] = useState<string | null>(null)
  const chosen = graph ?? graphs[0] ?? null
  const agents = (lastOf(useAsked(chosen === null ? null : { route: get_graph_cards, params: { name: chosen }, query: { space: ALL_SPACE, limit: AGENTS_OFFERED } }))?.cards ?? [])
    .map(card => ({ id: card.agent_id, label: card.title }))
  const [agent, setAgent] = useState(WHOLE_GRAPH)
  const picked = agents.some(each => each.id === agent) ? agent : WHOLE_GRAPH
  const fields = (about: string): ReactNode => (
    <>
      <Field kind="select" label="graph" value={chosen} options={graphs} onChange={value => { setGraph(textOf(value) || null); setAgent(WHOLE_GRAPH) }} />
      <Field kind="select" label="agent" value={picked} options={[WHOLE_GRAPH, ...agents.map(each => each.id)]}
        labels={Object.fromEntries<string>([[WHOLE_GRAPH, 'every agent in the graph'], ...agents.map((each): [string, string] => [each.id, each.label || each.id])])}
        onChange={value => { setAgent(textOf(value)) }} about={about} />
    </>
  )
  return { graph: chosen, agent: picked === WHOLE_GRAPH ? null : picked, grant: picked === WHOLE_GRAPH ? `graph:${chosen ?? ''}` : `agent:${picked}`, fields, none: graphs.length === 0 }
}

/** A standing grant to any agent, or to every agent in a graph, picked by title. */
function Assign({ pc, forFleet }: { readonly pc: PcRow; readonly forFleet: boolean }): ReactNode {
  const who = useGrantee()
  const assign = async (): Promise<Refused | null> => {
    if (who.graph === null) return null
    const failed = await assigning(pc.name, who.grant, true)()
    if (failed !== null || who.agent === null || forFleet) return failed
    return openDesk(who.graph, who.agent)
  }
  if (who.none) return <Quiet>no graph to assign it in</Quiet>
  return (
    <BoardForm label={`assign ${pc.name}`} onLeave={() => undefined}>
      {who.fields(`It acts on this PC with no share link, with desk/join(pc), across restarts, until unassigned.${forFleet ? '' : ' An agent is also given the desk server; every agent in a graph needs desk opened for the graph.'}`)}
      <BoardBar><Action label="assign" tone="primary" pending="assigning…" disabled={who.graph === null || pc.assigned.some(each => each.grantee === who.grant)} onInvoke={assign} /></BoardBar>
    </BoardForm>
  )
}

/** Lets an agent or a graph run commands on a RigDesk PC's agent shell, as the user or as SYSTEM, or takes that away (level null). */
const allowingShell = (pc: string, grantee: string, level: Level | null) => async (): Promise<Refused | null> => {
  const done = await routes('POST', 'shell_grant', { pc, grantee, level })
  return done.ok ? null : told(level === null ? 'shell grant not removed' : 'shell not allowed', done.refused)
}

const LEVELS: readonly Level[] = ['user', 'system']
const LEVEL_WORDS: Readonly<Record<Level, string>> = { user: 'as the user', system: 'as SYSTEM' }
const SWITCH_WORDS: Readonly<Record<string, string>> = { shell_off: 'off on the PC', shell_paused: 'paused by the person at the PC' }

type ShellRan = { readonly at_ms: number; readonly who: string; readonly graph: string | null; readonly as: Level | null; readonly command: string; readonly outcome: string; readonly took_ms: number | null; readonly job: string | null }

/** What agents ran there, from the PC's own journal, newest first; read while the row is open. */
function Ran({ pc, now }: { readonly pc: PcRow; readonly now: number }): ReactNode {
  const read = useRead<{ readonly ran: readonly ShellRan[] }>(`shell_ran?pc=${encodeURIComponent(pc.name)}`)
  const refused = refusedOf(read)
  const ran = valueOf(read)?.ran ?? null
  if (refused !== null) return <Refusal refused={refused} />
  if (ran === null) return <Quiet waiting>reading the PC's journal…</Quiet>
  if (ran.length === 0) return <Quiet>no agent has run a command here</Quiet>
  return (
    <BoardFacts label="commands run" terms="long" facts={ran.map((each): Fact => [`${agoOf(each.at_ms, now)} · ${each.who}`,
      <span key={`${String(each.at_ms)}-${each.command}`}>
        <code>{each.command}</code>{' '}
        <i className={css.dim} data-ink={each.outcome.startsWith('exit 0') || each.outcome === 'started' ? undefined : 'hold'}>
          {[each.as === null ? null : LEVEL_WORDS[each.as], each.outcome, each.took_ms === null ? null : durationOf(each.took_ms, 100), each.job].filter(Boolean).join(' · ')}
        </i>
      </span>])} />
  )
}

/** A RigDesk PC's agent shell: the PC's switch, who may run commands there with no session, a form to allow another, and what ran. */
function Shell({ pc, shell, now, forFleet }: { readonly pc: PcRow; readonly shell: PcShell; readonly now: number; readonly forFleet: boolean }): ReactNode {
  const who = useGrantee()
  const [level, setLevel] = useState<Level>('user')
  const allow = async (): Promise<Refused | null> => {
    if (who.graph === null) return null
    const failed = await allowingShell(pc.name, who.grant, level)()
    if (failed !== null || who.agent === null || forFleet) return failed
    return openDesk(who.graph, who.agent)
  }
  const switched = shell.switch
  return (
    <>
      <BoardBar>
        <span className={css.dim} data-ink={switched === null || switched.open ? undefined : 'hold'}>
          {switched === null ? 'this PC\'s RigDesk has no agent shell: update it' : switched.open ? 'on at the PC'
            : `${SWITCH_WORDS[switched.code ?? ''] ?? switched.code ?? 'closed'}${switched.words === null ? '' : `: ${switched.words}`}`}
        </span>
      </BoardBar>
      {shell.granted.map(each => (
        <BoardBar key={each.grantee} apart={<Action label="remove" tone="outlined" pending="removing…" onInvoke={allowingShell(pc.name, each.grantee, null)} />}>
          {each.agent_id !== null && each.graph !== null ? <Link href={href('agent', { graph: each.graph, agent: each.agent_id })}>{each.label}</Link> : <span>{each.label}</span>}
          <span className={css.dim}>{LEVEL_WORDS[each.level]}</span>
        </BoardBar>
      ))}
      {shell.granted.length === 0 ? <Quiet>no agent may run commands here without its session; the agent holding the session may, as the user</Quiet> : null}
      {who.none ? null : (
        <BoardForm label={`allow shell on ${pc.name}`} onLeave={() => undefined}>
          {who.fields(`It runs commands on this PC with desk/shell(pc), with no session and nothing on its screen.${forFleet ? '' : ' An agent is also given the desk server.'}`)}
          <Field kind="select" label="as" value={level} options={LEVELS} labels={LEVEL_WORDS} onChange={value => { setLevel(textOf(value) === 'system' ? 'system' : 'user') }}
            about="SYSTEM runs anything on the PC, off every desktop, and starts background jobs; the user runs as whoever is signed in." />
          <BoardBar><Action label="allow" tone="primary" pending="allowing…" disabled={who.graph === null || shell.granted.some(each => each.grantee === who.grant && each.level === level)} onInvoke={allow} /></BoardBar>
        </BoardForm>
      )}
      <Ran pc={pc} now={now} />
    </>
  )
}

/** One picture of the PC's screen, taken when asked and held only while the row is open; the PC journals it. */
function Screenshot({ pc, now }: { readonly pc: PcRow; readonly now: number }): ReactNode {
  const [shot, setShot] = useState<Shot | null>(null)
  const take = async (): Promise<Refused | null> => {
    const taken = await routes('POST', 'screenshot', { pc: pc.name })
    if (!taken.ok) return told('no screenshot', taken.refused)
    setShot(taken.value as Shot)
    return null
  }
  return (
    <>
      {shot === null ? null : <img className={css.shot} src={`data:${shot.mime};base64,${shot.data}`} alt={`${pc.name}'s screen ${agoOf(shot.taken_ms, now)}`} />}
      <BoardBar>
        <Action label={shot === null ? 'screenshot' : 'retake'} tone="outlined" pending="taking…" disabled={pc.state !== 'up'} onInvoke={take} />
        {shot === null ? null : <span className={css.dim}>{`taken ${agoOf(shot.taken_ms, now)}`}</span>}
      </BoardBar>
    </>
  )
}

/** Rig's switch for a PC's own MCP servers, kept in the PC's settings entry; off, Rig opens none of them. */
const allowing = (pc: string) => (on: boolean): Promise<Refused | null> => editSettings(document => withPath(document, ['plugins', 'rigdesk', 'pcs', pc, 'mcp'], on))

/** The MCP servers running on the PC, which the agent holding it calls as <pc>.<server>/<tool>: the switch, then each open server, or why there is none. */
function McpServers({ pc, mcp, now }: { readonly pc: PcRow; readonly mcp: Mcp; readonly now: number }): ReactNode {
  const none = mcp.why ?? (!mcp.allowed ? 'off: Rig opens none of this PC\'s servers' : mcp.on === null ? 'looked for when an agent holds the PC' : mcp.on ? 'the PC runs none' : 'off on the PC')
  return (
    <>
      <BoardBar>
        <label className={css.switch}><Toggle label={`${pc.name}'s MCP servers`} on={mcp.allowed} onTurn={allowing(pc.name)} />{mcp.allowed ? 'on' : 'off'}</label>
        <span className={css.dim}>{mcp.servers.length > 0 && mcp.why === null ? 'turning it off closes them for the agent holding the PC' : none}</span>
      </BoardBar>
      {mcp.servers.length === 0 ? null : (
        <BoardFacts label="MCP servers" facts={mcp.servers.map((each): Fact => [each.name,
          <span key={each.key}>{each.says} <i>{`· ${String(each.tools.length)} ${each.tools.length === 1 ? 'tool' : 'tools'} as ${each.key}/… · opened ${agoOf(each.since_ms, now)}`}</i></span>])} />
      )}
    </>
  )
}

type Sample = {
  readonly id: number
  readonly at_ms: number
  readonly rigdesk: string
  readonly took_ms: number
  readonly taken: 'ok' | 'failed'
  readonly why: string | null
  readonly figures: {
    readonly os: { readonly caption: string; readonly build: string }
    readonly uptime_s: number
    readonly cpu: { readonly name: string; readonly load_pct: number | null }
    readonly ram: { readonly total_mb: number; readonly free_mb: number }
    readonly volumes: readonly { readonly name: string; readonly label: string; readonly total_mb: number; readonly free_mb: number }[]
    readonly pending_reboot: boolean | null
    readonly pending_reboot_why: readonly string[]
    readonly updates: { readonly last_install_ms: number | null; readonly pending: number | null; readonly error: string | null } | null
    readonly defender: { readonly realtime_on: boolean | null; readonly signature_age_days: number | null } | null
    readonly bitlocker: readonly { readonly volume: string; readonly protection: string }[]
    readonly disks: readonly { readonly name: string; readonly health: string; readonly wear_pct: number | null; readonly temperature_c: number | null }[]
    readonly stopped_services: readonly { readonly name: string; readonly display: string; readonly exit_code: number | null }[]
    readonly system_errors_24h: number | null
    readonly battery: { readonly charge_pct: number | null; readonly status: string } | null
    readonly cpu_ms: number | null
    readonly errors: readonly string[]
  } | null
}
type Point = { readonly at_ms: number; readonly free_pct: Readonly<Record<string, number>> }
type HealthRead = { readonly sample: Sample | null; readonly series: readonly Point[] }

const HEALTH_EVERY_MS = 60_000
const MB = 1024 * 1024
const pct = (value: number): string => `${value.toFixed(0)}%`
const known = <T,>(value: T | null, words: (value: T) => string): string => (value === null ? 'unknown' : words(value))

/** A volume's free space over the week, as a line: the page draws what the PC reported, oldest at the left. */
function FreeLine({ name, series }: { readonly name: string; readonly series: readonly Point[] }): ReactNode {
  const points = series.flatMap(point => (point.free_pct[name] === undefined ? [] : [[point.at_ms, point.free_pct[name]] as const]))
  const first = points[0]?.[0] ?? 0
  const span = Math.max(1, (points.at(-1)?.[0] ?? 0) - first)
  if (points.length < 2) return null
  return (
    <svg className={css.line} viewBox="0 0 200 24" preserveAspectRatio="none" role="img" aria-label={`${name} free space over ${durationOf(span, 3_600_000)}`}>
      <polyline points={points.map(([at, free]) => `${((at - first) / span * 200).toFixed(1)},${(24 - free / 100 * 24).toFixed(1)}`).join(' ')} />
    </svg>
  )
}

/** The newest sample in full, each figure in text-mid unless a check on it fails; the week's free space beside each volume. */
function Health({ pc, now }: { readonly pc: PcRow; readonly now: number }): ReactNode {
  const read = useRead<HealthRead>(`health?pc=${encodeURIComponent(pc.name)}`, HEALTH_EVERY_MS)
  const refused = refusedOf(read)
  const view = valueOf(read)
  if (refused !== null) return <Refusal refused={refused} />
  if (view === null) return <Quiet waiting>reading…</Quiet>
  const sample = view.sample
  if (sample === null) return <Quiet>{pc.health?.why ?? 'no health sample yet: the PC takes one within a minute of starting, then every 10 minutes'}</Quiet>
  const failing = new Map((pc.health?.failing ?? []).map(check => [check.name, VERDICT_INK[check.state]]))
  const inked = (check: string, text: ReactNode): ReactNode => <span className={css.inked} data-ink={failing.get(check)}>{text}</span>
  const taken = `taken ${agoOf(sample.at_ms, now)} in ${durationOf(sample.took_ms, 100)}${sample.figures?.cpu_ms == null ? '' : `, ${durationOf(sample.figures.cpu_ms, 10)} of CPU`} · RigDesk ${sample.rigdesk}`
  const figures = sample.figures
  if (figures === null) return <BoardFacts label="health" facts={[['sample', taken], ['failed', inked('sample', sample.why ?? 'no reason given')]]} />
  const facts: readonly Fact[] = [
    ['sample', taken],
    ['Windows', `${figures.os.caption} ${figures.os.build} · up ${durationOf(figures.uptime_s * 1000, 60_000)}${figures.pending_reboot === true ? ` · reboot pending${figures.pending_reboot_why.length === 0 ? '' : ` (${figures.pending_reboot_why.join(', ')})`}` : ''}`],
    ['CPU', `${known(figures.cpu.load_pct, pct)} load · ${figures.cpu.name}`],
    ['RAM', `${bytesOf((figures.ram.total_mb - figures.ram.free_mb) * MB)} of ${bytesOf(figures.ram.total_mb * MB)} used`],
    ...figures.volumes.map((volume): Fact => [volume.name, (
      <span key={volume.name} className={css.volume}>
        {inked(`disk ${volume.name}`, `${bytesOf(volume.free_mb * MB)} free of ${bytesOf(volume.total_mb * MB)}${volume.total_mb === 0 ? '' : ` (${pct(100 * volume.free_mb / volume.total_mb)})`}${volume.label ? ` · ${volume.label}` : ''}`)}
        <FreeLine name={volume.name} series={view.series} />
      </span>
    )]),
    ['updates', figures.updates === null ? 'unknown' : figures.updates.error !== null ? `unknown: ${figures.updates.error}`
      : `${figures.updates.last_install_ms === null ? 'none installed' : `last installed ${agoOf(figures.updates.last_install_ms, now)}`} · ${known(figures.updates.pending, count => `${String(count)} pending`)}`],
    ['Defender', figures.defender === null ? 'unknown' : (
      <span key="defender">{inked('defender', `real-time ${known(figures.defender.realtime_on, on => (on ? 'on' : 'off'))}`)} · {inked('signatures', `signatures ${known(figures.defender.signature_age_days, days => `${String(days)} days old`)}`)}</span>
    )],
    ['BitLocker', figures.bitlocker.length === 0 ? 'none reported' : figures.bitlocker.map(each => `${each.volume} ${each.protection}`).join(', ')],
    ...figures.disks.map((disk): Fact => [disk.name, inked(`drive ${disk.name}`, [disk.health, disk.wear_pct === null ? null : `${String(disk.wear_pct)}% worn`,
      disk.temperature_c === null ? null : `${String(disk.temperature_c)} °C`].filter(Boolean).join(' · '))]),
    ['stopped services', figures.stopped_services.length === 0 ? 'none' : inked('services', figures.stopped_services.map(each => `${each.display || each.name}${each.exit_code === null || each.exit_code === 0 ? '' : ` (${String(each.exit_code)})`}`).join(', '))],
    ['System log', inked('system log', known(figures.system_errors_24h, count => `${String(count)} errors in 24 h`))],
    ...(figures.battery === null ? [] : [['battery', `${known(figures.battery.charge_pct, pct)} · ${figures.battery.status}`] as const]),
    ...(figures.errors.length === 0 ? [] : [['not read', figures.errors.join('; ')] as const]),
  ]
  return <BoardFacts label="health" facts={facts} />
}

type Verb = 'mint' | 'hand' | 'human' | 'stop' | 'block' | 'unblock' | 'revoke'

/** What a stock PC's human does to it, from here since it has no RigDesk viewer; a refusal is told with the verb that failed. */
const acting = (pc: PcRow, verb: Verb, minted?: (link: string) => void) => async (): Promise<Refused | null> => {
  const done = await routes('POST', 'viewer', { pc: pc.name, verb })
  if (!done.ok) return told(`${verb} failed`, done.refused)
  const link = (done.value as { readonly link?: string | null }).link
  if (verb === 'mint' && typeof link === 'string') {
    minted?.(link)
    copied('share link', link)
  }
  return null
}

/** A stock RustDesk PC has no RigDesk viewer, so its human's buttons are here: share it with an agent, hand input over, block, end the share. Stop is on the row. */
function StockButtons({ pc, minted }: { readonly pc: PcRow; readonly minted: (link: string) => void }): ReactNode {
  const owned = pc.owner !== null
  const sharer = pc.owner?.id == null ? null : `agent:${pc.owner.id}`
  return (
    <BoardBar apart={owned ? (
      <>
        {pc.holder === 'blocked' ? <Action label="unblock" tone="outlined" onInvoke={acting(pc, 'unblock')} /> : <Action label="block" tone="outlined" onInvoke={acting(pc, 'block')} />}
        <Action label="end share" tone="danger" onInvoke={acting(pc, 'revoke')} />
      </>
    ) : undefined}>
      <Action label="share with agent" tone="outlined" pending="minting…" disabled={pc.state !== 'up'} onInvoke={acting(pc, 'mint', minted)} />
      {sharer !== null && !pc.assigned.some(each => each.grantee === sharer) ? <Action label="assign this agent" tone="outlined" pending="assigning…" onInvoke={assigning(pc.name, sharer, true)} /> : null}
      {owned && pc.holder === 'human' ? <Action label="hand to agent" tone="outlined" onInvoke={acting(pc, 'hand')} /> : null}
    </BoardBar>
  )
}

/** One PC as a row: its name, where it stands and who holds input, who is on it or why it is down, who is assigned, and stop while an agent is on it. */
function Pc({ pc, now, forFleet }: { readonly pc: PcRow; readonly now: number; readonly forFleet: boolean }): ReactNode {
  const [open, setOpen] = useRememberedFold('rigdesk-pc', pc.name, 'closed')
  const [link, setLink] = useState<string | null>(null)
  const standing = standingOf(pc)
  const ink = inkOf(standing, pc)
  const health = pc.health ?? null
  const headline = health?.figures ?? null
  const verdictWhy = standing in VERDICT_INK ? health?.failing[0]?.words : undefined
  const failingInk = new Map((health?.failing ?? []).map(check => [check.name, VERDICT_INK[check.state]]))
  const held = pc.owner === null ? null : <span>held by <OwnerOf owner={pc.owner} />{pc.owned_here ? '' : ' (joined elsewhere)'}</span>
  const assigned = pc.assigned.map(each => each.label)
  const servers = pc.mcp?.servers.length ?? 0
  const facts: readonly Fact[] = [
    ['client', pc.client || 'none'], ['kind', pc.stock ? 'stock RustDesk' : 'RigDesk'], ['address', pc.url], ['input', pc.holder === null ? 'no agent' : HOLDER[pc.holder]],
    ...(held === null ? [] : [['agent', held] as const]),
    ...(pc.down_since_ms === null ? [] : [['down since', new Date(pc.down_since_ms).toLocaleString()] as const]),
    ...(health === null ? [] : [['health', health.state === null ? 'no sample yet' : `${health.state}${health.sample_ms === null ? '' : `, sampled ${agoOf(health.sample_ms, now)}`}`] as const]),
  ]
  const problems = [...(pc.state === 'down' && pc.why !== null ? [pc.why] : []), ...(health?.why == null ? [] : [health.why]),
    ...(health?.failing ?? []).filter(check => check.state === 'fault').map(check => check.words)]
  return (
    <BoardRow label={pc.name} open={open} onOpen={setOpen} mark={<Mark kind="status" shape="square" label={standing} ink={ink} fill={pc.state === 'up' ? 'solid' : 'open'} />}
      name={pc.name} under={[pc.client, pc.stock ? 'stock RustDesk' : `RigDesk${health?.rigdesk == null ? '' : ` ${health.rigdesk}`}`].filter(Boolean).join(' · ')}
      state={{ word: standing, ink: standing === 'up' ? 'idle' : ink }}
      why={pc.down_since_ms !== null ? agoOf(pc.down_since_ms, now) : verdictWhy ?? (pc.holder === null ? undefined : HOLDER[pc.holder])}
      telling={pc.state === 'down' ? pc.why ?? 'no reason given' : held ?? 'no agent on it'} tellingDim={pc.state !== 'down' && held === null}
      figures={headline !== null ? {
        main: <>{headline.volumes.map((volume, at) => <span key={volume.name} className={css.inked} data-ink={failingInk.get(`disk ${volume.name}`)}>{`${at === 0 ? '' : ' · '}${volume.name} ${pct(volume.free_pct)}`}</span>)}{' free'}</>,
        under: [`RAM ${known(headline.ram_used_pct, pct)}`, `CPU ${known(headline.cpu_pct, pct)}`, headline.build, `up ${durationOf(headline.uptime_s * 1000, 3_600_000)}`,
          headline.pending_reboot === true ? 'reboot pending' : null].filter(Boolean).join(' · '),
      } : !pc.stock ? (servers === 0 ? undefined : { main: `${String(servers)} MCP ${servers === 1 ? 'server' : 'servers'}` }) : assigned.length === 0 ? { main: 'assigned to no one', dim: true }
        : { main: assigned.length === 1 ? `assigned to ${assigned[0] ?? ''}` : `${String(assigned.length)} assigned`, under: assigned.length === 1 ? undefined : assigned.join(', ') }}
      verbs={pc.stock && pc.owner !== null ? <Action label="stop" tone="danger" onInvoke={acting(pc, 'stop')} /> : undefined}>
      <BoardProblems problems={problems} />
      <BoardFacts facts={facts} />
      {link === null || pc.owner !== null ? null : (
        <BoardPart name="share link">
          <Text text={link} format="code" size="sm" />
          <BoardBar><Action label="copy" lead="copy" tone="outlined" onInvoke={() => { copied('share link', link); return Promise.resolve(null) }} /><span className={css.dim}>works once</span></BoardBar>
        </BoardPart>
      )}
      {pc.stock ? <BoardPart name="assign"><Assign pc={pc} forFleet={forFleet} /></BoardPart> : null}
      {pc.assigned.length === 0 ? null : <BoardPart name="assigned"><Grants pc={pc} /></BoardPart>}
      {pc.stock || pc.mcp === undefined ? null : <BoardPart name="MCP"><McpServers pc={pc} mcp={pc.mcp} now={now} /></BoardPart>}
      {health === null ? null : <BoardPart name="health"><Health pc={pc} now={now} /></BoardPart>}
      {pc.shell == null ? null : <BoardPart name="agent shell"><Shell pc={pc} shell={pc.shell} now={now} forFleet={forFleet} /></BoardPart>}
      <BoardPart name="screen"><Screenshot pc={pc} now={now} /></BoardPart>
      {pc.stock ? <StockButtons pc={pc} minted={setLink} /> : <Quiet>its human shares it from the RigDesk viewer</Quiet>}
    </BoardRow>
  )
}

/** Our own hbbs or hbbr, which stock PCs named by ID register with and meet the bridge on: up when it took a TCP connection at the last probe. */
function ListenerRow({ each, now }: { readonly each: Listener; readonly now: number }): ReactNode {
  const [open, setOpen] = useRememberedFold('rigdesk-listener', each.role, 'closed')
  const ink = LISTENER_INK[each.state]
  return (
    <BoardRow label={ROLE[each.role]} open={open} onOpen={setOpen} mark={<Mark kind="status" shape="square" label={each.state} ink={ink} fill={each.state === 'up' ? 'solid' : 'open'} />}
      name={ROLE[each.role]} under={each.role} state={{ word: each.state === 'unchecked' ? 'not checked' : each.state, ink: each.state === 'up' ? 'idle' : ink }}
      why={each.since_ms === null ? undefined : agoOf(each.since_ms, now)} telling={each.why ?? each.address} tellingDim={each.why === null}
      figures={{ main: each.checked_ms === null ? 'never checked' : `checked ${agoOf(each.checked_ms, now)}`, dim: true }}>
      <BoardProblems problems={each.state === 'down' && each.why !== null ? [each.why] : []} />
      <BoardFacts facts={[['address', each.address], ['service', each.role], ...(each.since_ms === null ? [] : [[`${each.state} since`, new Date(each.since_ms).toLocaleString()] as const])]} />
    </BoardRow>
  )
}

/** A stock RustDesk PC by ID: the install line puts a Windows PC on our ID server and prints its ID and password, which this form hands to Rig. */
function Adding({ server, taken, close }: { readonly server: IdServer; readonly taken: ReadonlySet<string>; readonly close: () => void }): ReactNode {
  const [name, setName] = useState('')
  const [client, setClient] = useState('')
  const [id, setId] = useState('')
  const [password, setPassword] = useState('')
  const named = name.trim()
  const rustdeskId = id.replace(/\s+/g, '')
  const nameWrong = named === '' ? null : !NAME.test(named) ? 'lowercase letters, digits and - only' : taken.has(named) ? `${named} is taken` : null
  const idWrong = rustdeskId === '' || RUSTDESK_ID.test(rustdeskId) ? null : 'digits only, as RustDesk shows it'
  const passwordWrong = password === '' || PASSWORD.test(password) ? null : '6 to 128 characters, no spaces'
  const save = async (): Promise<Refused | null> => {
    const added = await routes('POST', 'pc', { name: named, id: rustdeskId, password, client: client.trim() })
    if (!added.ok) return told('PC not added', added.refused)
    const { entry } = added.value as PcAdded
    const failed = await editSettings(document => withPath(document, ['plugins', 'rigdesk', 'pcs', named], entry as never))
    if (failed !== null) return failed
    close()
    return null
  }
  const install = server.install
  return (
    <BoardNew>
      {install === null ? <BoardProblems problems={[`no install line: ${server.install_why ?? 'the ID server is not readable'}`]} /> : (
        <BoardPart name="install line">
          <Text text={install} format="code" size="sm" />
          <BoardBar>
            <Action label="copy" lead="copy" tone="outlined" onInvoke={() => { copied('install line', install); return Promise.resolve(null) }} />
            <span className={css.dim}>run it on the Windows PC, in PowerShell as administrator; it prints the ID and password</span>
          </BoardBar>
        </BoardPart>
      )}
      <BoardForm label="add a PC" onLeave={close}>
        <Field kind="text" label="name" value={name} placeholder="front-desk" invalid={nameWrong} onChange={value => { setName(textOf(value)) }}
          about="What agents and this page call it: desk/pcs lists it, desk/request and desk/join name it." />
        <Field kind="text" label="client" value={client} placeholder="acme" onChange={value => { setClient(textOf(value)) }} about="Who the PC belongs to; optional." />
        <Field kind="text" label="RustDesk ID" value={id} placeholder="123 456 789" invalid={idWrong} onChange={value => { setId(textOf(value)) }} about="As the install line printed it." />
        <Field kind="text" label="password" value={password} placeholder="as printed" invalid={passwordWrong} onChange={value => { setPassword(textOf(value)) }}
          about="The PC's permanent RustDesk password. Rig keeps it in a file of its own; it never reaches settings." />
        <BoardBar>
          <Action label="add" tone="primary" pending="adding…" submits disabled={!named || nameWrong !== null || !rustdeskId || idWrong !== null || !password || passwordWrong !== null} onInvoke={save} />
          <Action label="cancel" tone="outlined" onInvoke={() => { close(); return Promise.resolve(null) }} />
        </BoardBar>
      </BoardForm>
    </BoardNew>
  )
}

export function RigdeskPage(): ReactNode {
  const read = useRead<PcsView>('pcs')
  const now = useNow()
  const [adding, setAdding] = useState(false)
  const refused = refusedOf(read)
  const view = valueOf(read)
  const pcs = (view?.pcs ?? []).toSorted(byStanding)
  const server = view?.id_server ?? null
  const up = pcs.filter(pc => pc.state === 'up').length
  const rows = (
    <BoardRows label="PCs">
      {server !== null && adding ? <Adding server={server} taken={new Set(pcs.map(pc => pc.name))} close={() => { setAdding(false) }} /> : null}
      {pcs.map(pc => <Pc key={pc.name} pc={pc} now={now} forFleet={view?.for_fleet === true} />)}
    </BoardRows>
  )
  const none = view === null || pcs.length > 0 || adding ? null : <Quiet>{server === null ? 'no PCs, and no ID server to add one on: name it under plugins.rigdesk.id_server' : 'no PCs'}</Quiet>
  return (
    <Board label="RigDesk" title="RigDesk" columns={COLUMNS}
      actions={server === null ? undefined : <Action label="add" lead="plus" tone="outlined" disabled={adding} onInvoke={() => { setAdding(true); return Promise.resolve(null) }} />}>
      {refused === null ? null : <Refusal refused={refused} />}
      {view === null && refused === null ? <Quiet waiting>reading…</Quiet> : null}
      {server === null ? <>{rows}{none}</> : (
        <>
          <Headed title="PCs" summary={pcs.length === 0 ? null : `${String(up)} of ${String(pcs.length)} up`}>{rows}{none}</Headed>
          <Headed title="ID server" summary={server.host}><BoardRows label="ID server">{server.listeners.map(each => <ListenerRow key={each.role} each={each} now={now} />)}</BoardRows></Headed>
        </>
      )}
    </Board>
  )
}
