import { useEffect, useState, type ReactNode } from 'react'
import { ALL_SPACE, get_agent_settings, get_graph_cards, get_graphs, patch_agent_settings } from '@rig/contract'
import { Action, agoOf, AppTab, Board, BoardBar, BoardFacts, BoardForm, BoardNew, BoardPart, BoardProblems, BoardRow, BoardRows, contribution, editSettings, Field, Headed, href, lastOf, Link, Mark,
  objectOf, Quiet, REREAD_MS, Refusal, store as core, tell, Text, told, useAsked, useNow, useRememberedFold, withPath, type BoardColumns, type Fact, type Ink, type Refused } from '@rig/host'
import css from './rigdesk.module.css'

const routes = contribution('rigdesk')
const EVERY_MS = Math.min(REREAD_MS, 5_000)

type Owner = { readonly id: string | null; readonly title: string | null; readonly graph: string | null }
type Grant = { readonly grantee: string; readonly label: string; readonly graph: string | null; readonly agent_id: string | null }
type Shot = { readonly mime: string; readonly data: string; readonly taken_ms: number }
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
type PcsView = { readonly pcs: readonly PcRow[]; readonly id_server: IdServer | null; readonly for_fleet: boolean; readonly source: string }
type PcAdded = { readonly name: string; readonly entry: Readonly<Record<string, unknown>> }

const INK: Readonly<Record<PcRow['state'], Ink>> = { up: 'live', connecting: 'hold', down: 'fault' }
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

/** A PC's row: its client and kind under its name, who holds input under its state, who is on it or why it is down, who is assigned. */
const COLUMNS: BoardColumns = { who: '30ch', how: '20ch', figures: '26ch', end: '8ch' }
/** What needs the operator sorts first, what is broken last. */
const ORDER: Readonly<Record<string, number>> = { blocked: 0, held: 1, up: 2, connecting: 3, down: 4 }
const standingOf = (pc: PcRow): string => (pc.holder === 'blocked' ? 'blocked' : pc.state === 'up' && pc.owner !== null ? 'held' : pc.state)
const rankOf = (pc: PcRow): number => ORDER[standingOf(pc)] ?? 2
const byStanding = (one: PcRow, other: PcRow): number => {
  const apart = rankOf(one) - rankOf(other)
  return apart === 0 ? one.name.localeCompare(other.name) : apart
}
const copied = (what: string, text: string): void => {
  void navigator.clipboard.writeText(text).then(() => { tell({ key: `rigdesk:${what}`, tone: 'info', source: 'RigDesk', message: `${what} copied` }) },
    () => { tell({ key: `rigdesk:${what}`, tone: 'fault', source: 'RigDesk', message: `${what} not copied: the browser refused the clipboard` }) })
}

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
    body: { settings: { mcp_servers: { ...servers, [DESK_SERVER]: 'server' } }, expected: { mcp_servers: servers } } })
  return saved.ok ? null : told('assigned, but desk not opened for it', saved.refused)
}

/** A standing grant to any agent, or to every agent in a graph, picked by title. */
function Assign({ pc, forFleet }: { readonly pc: PcRow; readonly forFleet: boolean }): ReactNode {
  const graphs = (lastOf(useAsked(GRAPHS)) ?? []).flatMap(row => (row.type === 'graph' && !row.flow ? [row.name] : []))
  const [graph, setGraph] = useState<string | null>(null)
  const chosen = graph ?? graphs[0] ?? null
  const agents = (lastOf(useAsked(chosen === null ? null : { route: get_graph_cards, params: { name: chosen }, query: { space: ALL_SPACE, limit: AGENTS_OFFERED } }))?.cards ?? [])
    .map(card => ({ id: card.agent_id, label: card.title }))
  const [agent, setAgent] = useState(WHOLE_GRAPH)
  const picked = agents.some(each => each.id === agent) ? agent : WHOLE_GRAPH
  const grant = picked === WHOLE_GRAPH ? `graph:${chosen ?? ''}` : `agent:${picked}`
  const assign = async (): Promise<Refused | null> => {
    if (chosen === null) return null
    const failed = await assigning(pc.name, grant, true)()
    if (failed !== null || picked === WHOLE_GRAPH || forFleet) return failed
    return openDesk(chosen, picked)
  }
  if (graphs.length === 0) return <Quiet>no graph to assign it in</Quiet>
  return (
    <BoardForm label={`assign ${pc.name}`} onLeave={() => undefined}>
      <Field kind="select" label="graph" value={chosen} options={graphs} onChange={value => { setGraph(textOf(value) || null); setAgent(WHOLE_GRAPH) }} />
      <Field kind="select" label="agent" value={picked} options={[WHOLE_GRAPH, ...agents.map(each => each.id)]}
        labels={Object.fromEntries<string>([[WHOLE_GRAPH, 'every agent in the graph'], ...agents.map((each): [string, string] => [each.id, each.label || each.id])])}
        onChange={value => { setAgent(textOf(value)) }}
        about={`It acts on this PC with no share link, with desk/join(pc), across restarts, until unassigned.${forFleet ? '' : ' An agent is also given the desk server; every agent in a graph needs desk opened for the graph.'}`} />
      <BoardBar><Action label="assign" tone="primary" pending="assigning…" disabled={chosen === null || pc.assigned.some(each => each.grantee === grant)} onInvoke={assign} /></BoardBar>
    </BoardForm>
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
  const ink: Ink = standing === 'blocked' ? 'fault' : INK[pc.state]
  const held = pc.owner === null ? null : <span>held by <OwnerOf owner={pc.owner} />{pc.owned_here ? '' : ' (joined elsewhere)'}</span>
  const assigned = pc.assigned.map(each => each.label)
  const facts: readonly Fact[] = [
    ['client', pc.client || 'none'], ['kind', pc.stock ? 'stock RustDesk' : 'RigDesk'], ['address', pc.url], ['input', pc.holder === null ? 'no agent' : HOLDER[pc.holder]],
    ...(held === null ? [] : [['agent', held] as const]),
    ...(pc.down_since_ms === null ? [] : [['down since', new Date(pc.down_since_ms).toLocaleString()] as const]),
  ]
  return (
    <BoardRow label={pc.name} open={open} onOpen={setOpen} mark={<Mark kind="status" shape="square" label={standing} ink={ink} fill={pc.state === 'up' ? 'solid' : 'open'} />}
      name={pc.name} under={[pc.client, pc.stock ? 'stock RustDesk' : 'RigDesk'].filter(Boolean).join(' · ')}
      state={{ word: standing, ink: standing === 'up' ? 'idle' : ink }}
      why={pc.down_since_ms !== null ? agoOf(pc.down_since_ms, now) : pc.holder === null ? undefined : HOLDER[pc.holder]}
      telling={pc.state === 'down' ? pc.why ?? 'no reason given' : held ?? 'no agent on it'} tellingDim={pc.state !== 'down' && held === null}
      figures={!pc.stock ? { main: 'shared by link', dim: true } : assigned.length === 0 ? { main: 'assigned to no one', dim: true }
        : { main: assigned.length === 1 ? `assigned to ${assigned[0] ?? ''}` : `${String(assigned.length)} assigned`, under: assigned.length === 1 ? undefined : assigned.join(', ') }}
      verbs={pc.stock && pc.owner !== null ? <Action label="stop" tone="danger" onInvoke={acting(pc, 'stop')} /> : undefined}>
      <BoardProblems problems={pc.state === 'down' && pc.why !== null ? [pc.why] : []} />
      <BoardFacts facts={facts} />
      {link === null || pc.owner !== null ? null : (
        <BoardPart name="share link">
          <Text text={link} format="code" size="sm" />
          <BoardBar><Action label="copy" lead="copy" tone="outlined" onInvoke={() => { copied('share link', link); return Promise.resolve(null) }} /><span className={css.dim}>works once</span></BoardBar>
        </BoardPart>
      )}
      {pc.stock ? <BoardPart name="assign"><Assign pc={pc} forFleet={forFleet} /></BoardPart> : null}
      {pc.assigned.length === 0 ? null : <BoardPart name="assigned"><Grants pc={pc} /></BoardPart>}
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
  const read = usePcs()
  const now = useNow()
  const [adding, setAdding] = useState(false)
  const refused = read !== null && 'rendered' in read ? read : null
  const view = read !== null && !('rendered' in read) ? read : null
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
      <Quiet>{`RigDesk is AGPL-3.0; its source is at ${view?.source ?? 'https://github.com/deussalo/rigdesk'}`}</Quiet>
    </Board>
  )
}
