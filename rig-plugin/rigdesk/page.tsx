import { useEffect, useState, type ReactNode } from 'react'
import { Action, agoOf, AppTab, contribution, Create, editSettings, Field, href, IconButton, Link, Mark, Panel, REREAD_MS, Refusal, Row, Stack, tell, Text, told, useNow, withPath, type Ink, type Refused } from '@rig/host'

const routes = contribution('rigdesk')
const EVERY_MS = Math.min(REREAD_MS, 5_000)

type Owner = { readonly id: string | null; readonly title: string | null; readonly graph: string | null }
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
  readonly assigned: readonly string[]
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
type PcsView = { readonly pcs: readonly PcRow[]; readonly id_server: IdServer | null; readonly source: string }
type PcAdded = { readonly name: string; readonly entry: Readonly<Record<string, unknown>> }

const INK: Readonly<Record<PcRow['state'], Ink>> = { up: 'live', connecting: 'hold', down: 'fault' }
const LISTENER_INK: Readonly<Record<Listener['state'], Ink>> = { up: 'live', unchecked: 'hold', down: 'fault' }
const ROLE: Readonly<Record<Listener['role'], string>> = { hbbs: 'ID server (hbbs)', hbbr: 'relay (hbbr)' }
const NAME = /^[a-z0-9][a-z0-9-]{0,62}$/
const RUSTDESK_ID = /^[0-9]{6,32}$/
const PASSWORD = /^[\x21-\x7e]{6,128}$/
const HOLDER: Readonly<Record<NonNullable<PcRow['holder']>, string>> = { human: 'human holds input', agent: 'agent holds input', blocked: 'agent blocked' }

export function RigdeskTab(): ReactNode {
  return <AppTab plugin="rigdesk" path="all" label="RigDesk" icon="bento" />
}

function usePcs(): PcsView | Refused | null {
  const [read, setRead] = useState<PcsView | Refused | null>(null)
  useEffect(() => {
    let live = true
    const once = async (): Promise<void> => {
      const result = await routes('GET', 'pcs')
      if (live) setRead(result.ok ? result.value as PcsView : result.refused)
    }
    void once()
    const timer = setInterval(() => { void once() }, EVERY_MS)
    return () => { live = false; clearInterval(timer) }
  }, [])
  return read
}

function OwnerOf({ owner }: { readonly owner: Owner }): ReactNode {
  const title = owner.title ?? owner.id ?? 'an agent'
  return owner.graph !== null && owner.id !== null ? <Link href={href('agent', { graph: owner.graph, agent: owner.id })}>{title}</Link> : <>{title}</>
}

/** A standing grant, kept by rigdesk: the agent acts on this stock PC with no share link, across restarts, until unassigned. */
const assigning = (pc: string, grantee: string, granted: boolean) => async (): Promise<Refused | null> => {
  const done = await routes('POST', 'assign', { pc, grantee, granted })
  return done.ok ? null : told(granted ? 'assign failed' : 'unassign failed', done.refused)
}

/** Who holds a standing grant to a stock PC, each with its unassign; the PCs page assigns any agent or graph. */
function Grants({ pc }: { readonly pc: PcRow }): ReactNode {
  if (pc.assigned.length === 0) return <Text text="Assigned to no one: only a share link reaches it. Assign the agent sharing it here, or any agent on the PCs page." />
  return (
    <Stack gap="sm">
      {pc.assigned.map(each => (
        <Row key={each} title={each === `agent:${pc.owner?.id ?? ''}` ? `${pc.owner?.title ?? each} · ${each}` : each} word="assigned"
          actions={<Action label="unassign" tone="danger" pending="unassigning…" onInvoke={assigning(pc.name, each, false)} />} />
      ))}
    </Stack>
  )
}

type Verb = 'mint' | 'hand' | 'human' | 'stop' | 'block' | 'unblock' | 'revoke'

/** A stock RustDesk PC has no RigDesk viewer, so its human's buttons are here: share it with an agent, hand or take back input, block, end the share. */
function StockButtons({ pc, minted }: { readonly pc: PcRow; readonly minted: (link: string) => void }): ReactNode {
  const act = (verb: Verb) => async (): Promise<Refused | null> => {
    const done = await routes('POST', 'viewer', { pc: pc.name, verb })
    if (!done.ok) return told(`${verb} failed`, done.refused)
    const link = (done.value as { readonly link?: string | null }).link
    if (verb === 'mint' && typeof link === 'string') {
      minted(link)
      void navigator.clipboard?.writeText(link).catch(() => undefined)
    }
    return null
  }
  const owned = pc.owner !== null
  const sharer = pc.owner?.id == null ? null : `agent:${pc.owner.id}`
  return (
    <>
      <Action label="share with agent" tone="primary" pending="minting…" disabled={pc.state !== 'up'} onInvoke={act('mint')} />
      {sharer !== null && !pc.assigned.includes(sharer) ? <Action label="assign this agent" pending="assigning…" onInvoke={assigning(pc.name, sharer, true)} /> : null}
      {owned && pc.holder === 'human' ? <Action label="hand to agent" onInvoke={act('hand')} /> : null}
      {owned ? <Action label="stop" tone="danger" onInvoke={act('stop')} /> : null}
      {owned && pc.holder !== 'blocked' ? <Action label="block" tone="danger" onInvoke={act('block')} /> : null}
      {owned && pc.holder === 'blocked' ? <Action label="unblock" onInvoke={act('unblock')} /> : null}
      {owned ? <Action label="end share" tone="danger" onInvoke={act('revoke')} /> : null}
    </>
  )
}

/** Our own hbbs and hbbr, which stock PCs named by ID register with and meet the bridge on: each is up when it took a TCP connection at the last probe. */
function Health({ server, now }: { readonly server: IdServer; readonly now: number }): ReactNode {
  return (
    <>
      {server.listeners.map(each => (
        <Row key={each.role} glyph={<Mark kind="status" shape="square" label={each.state} ink={LISTENER_INK[each.state]} fill="open" />}
          title={ROLE[each.role]} word={each.state} meta={each.address}
          aside={each.since_ms === null ? undefined : `${each.state} ${agoOf(each.since_ms, now)}`}
          preview={each.why ?? undefined} />
      ))}
    </>
  )
}

const textOf = (value: unknown): string => (typeof value === 'string' ? value : '')

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
  const copy = (): void => {
    if (server.install === null) return
    void navigator.clipboard.writeText(server.install).then(() => { tell({ key: 'rigdesk:install', tone: 'info', source: 'RigDesk', message: 'install line copied' }) },
      () => { tell({ key: 'rigdesk:install', tone: 'fault', source: 'RigDesk', message: 'not copied: the browser refused the clipboard' }) })
  }
  const save = async (): Promise<Refused | null> => {
    const added = await routes('POST', 'pc', { name: named, id: rustdeskId, password, client: client.trim() })
    if (!added.ok) return told('PC not added', added.refused)
    const { entry } = added.value as PcAdded
    const failed = await editSettings(document => withPath(document, ['plugins', 'rigdesk', 'pcs', named], entry as never))
    if (failed !== null) return failed
    close()
    return null
  }
  return (
    <Stack gap="sm">
      {server.install === null ? <Text text={`No install line: ${server.install_why ?? 'the ID server is not readable'}.`} ink="fault" /> : <>
        <Text text="1. On the Windows PC, in PowerShell run as administrator, paste this line. It installs stock RustDesk (pinned and checked), points it at our ID server and prints the PC's ID and a fresh password." />
        <Stack direction="row" gap="sm" align="center">
          <Text text={server.install} format="code" size="sm" />
          <IconButton icon="copy" label="copy the install line" onClick={copy} />
        </Stack>
      </>}
      <Text text="2. Name the PC and enter what the line printed. Rig keeps the password in a file of its own and starts the PC's rigdesk-bridge; the PC shows up here within seconds." />
      <Field kind="text" label="name" value={name} placeholder="front-desk" invalid={nameWrong} onChange={value => { setName(textOf(value)) }}
        about="What agents and this page call it: desk/pcs lists it, desk/request and desk/join name it." />
      <Field kind="text" label="client" value={client} placeholder="acme" onChange={value => { setClient(textOf(value)) }} about="Who the PC belongs to; optional." />
      <Field kind="text" label="RustDesk ID" value={id} placeholder="123 456 789" invalid={idWrong} onChange={value => { setId(textOf(value)) }} />
      <Field kind="text" label="password" value={password} placeholder="as printed" invalid={passwordWrong} onChange={value => { setPassword(textOf(value)) }}
        about="The PC's permanent RustDesk password. It never reaches settings." />
      <Stack direction="row" gap="sm" align="center">
        <Action label="add PC" tone="primary" pending="adding…" disabled={!named || nameWrong !== null || !rustdeskId || idWrong !== null || !password || passwordWrong !== null} onInvoke={save} />
        <Action label="cancel" onInvoke={() => { close(); return Promise.resolve(null) }} />
      </Stack>
    </Stack>
  )
}

export function RigdeskPage(): ReactNode {
  const read = usePcs()
  const now = useNow()
  const [links, setLinks] = useState<Readonly<Record<string, string>>>({})
  const [adding, setAdding] = useState(false)
  if (read !== null && 'rendered' in read) return <Refusal refused={read} />
  const pcs = read?.pcs ?? []
  const server = read?.id_server ?? null
  return (
    <Panel title="RigDesk" actions={server === null || adding ? undefined : <Create label="add a PC" onCreate={() => { setAdding(true) }} />}>
      <Stack gap="sm">
        {server === null ? null : <Health server={server} now={now} />}
        {server !== null && adding ? <Adding server={server} taken={new Set(pcs.map(pc => pc.name))} close={() => { setAdding(false) }} /> : null}
        {pcs.length === 0 ? <Text text={server === null ? 'No RigDesk PC is set up. Name the ID server under plugins.rigdesk.id_server to add stock PCs here, or a RigDesk PC under plugins.rigdesk.pcs.' : 'No RigDesk PC is set up: add a PC above.'} /> : null}
        {pcs.map(pc => (
          <Row key={pc.name} glyph={<Mark kind="status" shape="square" label={pc.state} ink={pc.holder === 'blocked' ? 'fault' : INK[pc.state]} fill="open" />}
            title={pc.name} word={pc.state}
            aside={pc.down_since_ms === null ? undefined : `down ${agoOf(pc.down_since_ms, now)}`}
            meta={[pc.client, pc.stock ? `stock RustDesk via ${pc.url}` : pc.url, pc.holder === null ? 'no agent' : HOLDER[pc.holder]].filter(Boolean).join(' · ')}
            preview={pc.owner === null ? (pc.why ?? undefined) : <>owned by <OwnerOf owner={pc.owner} />{pc.owned_here ? '' : ' (joined elsewhere)'}</>}
            foot={pc.stock ? <>
              {links[pc.name] !== undefined && pc.owner === null ? <Text text={`Give this link to an agent (copied; it works once): ${links[pc.name] ?? ''}`} /> : null}
              <Grants pc={pc} />
            </> : undefined}
            actions={pc.stock ? <StockButtons pc={pc} minted={link => { setLinks(held => ({ ...held, [pc.name]: link })) }} /> : undefined} />
        ))}
        <Text text={`RigDesk is AGPL-3.0; its source is at ${read?.source ?? 'https://github.com/deussalo/rigdesk'}.`} />
      </Stack>
    </Panel>
  )
}
