import { useEffect, useState, type ReactNode } from 'react'
import { Action, agoOf, AppTab, contribution, href, Link, Mark, Panel, REREAD_MS, Refusal, Row, Stack, Text, told, useNow, type Ink, type Refused } from '@rig/host'

const server = contribution('rigdesk')
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
}
type PcsView = { readonly pcs: readonly PcRow[]; readonly source: string }

const INK: Readonly<Record<PcRow['state'], Ink>> = { up: 'live', connecting: 'hold', down: 'fault' }
const HOLDER: Readonly<Record<NonNullable<PcRow['holder']>, string>> = { human: 'human holds input', agent: 'agent holds input', blocked: 'agent blocked' }

export function RigdeskTab(): ReactNode {
  return <AppTab plugin="rigdesk" path="all" label="RigDesk" icon="bento" />
}

function usePcs(): PcsView | Refused | null {
  const [read, setRead] = useState<PcsView | Refused | null>(null)
  useEffect(() => {
    let live = true
    const once = async (): Promise<void> => {
      const result = await server('GET', 'pcs')
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

type Verb = 'mint' | 'hand' | 'human' | 'stop' | 'block' | 'unblock' | 'revoke'

/** A stock RustDesk PC has no RigDesk viewer, so its human's buttons are here: share it with an agent, hand or take back input, block, end the share. */
function StockButtons({ pc, minted }: { readonly pc: PcRow; readonly minted: (link: string) => void }): ReactNode {
  const act = (verb: Verb) => async (): Promise<Refused | null> => {
    const done = await server('POST', 'viewer', { pc: pc.name, verb })
    if (!done.ok) return told(`${verb} failed`, done.refused)
    const link = (done.value as { readonly link?: string | null }).link
    if (verb === 'mint' && typeof link === 'string') {
      minted(link)
      void navigator.clipboard?.writeText(link).catch(() => undefined)
    }
    return null
  }
  const owned = pc.owner !== null
  return (
    <>
      <Action label="share with agent" tone="primary" pending="minting…" disabled={pc.state !== 'up'} onInvoke={act('mint')} />
      {owned && pc.holder === 'human' ? <Action label="hand to agent" onInvoke={act('hand')} /> : null}
      {owned ? <Action label="stop" tone="danger" onInvoke={act('stop')} /> : null}
      {owned && pc.holder !== 'blocked' ? <Action label="block" tone="danger" onInvoke={act('block')} /> : null}
      {owned && pc.holder === 'blocked' ? <Action label="unblock" onInvoke={act('unblock')} /> : null}
      {owned ? <Action label="end share" tone="danger" onInvoke={act('revoke')} /> : null}
    </>
  )
}

export function RigdeskPage(): ReactNode {
  const read = usePcs()
  const now = useNow()
  const [links, setLinks] = useState<Readonly<Record<string, string>>>({})
  if (read !== null && 'rendered' in read) return <Refusal refused={read} />
  const pcs = read?.pcs ?? []
  return (
    <Panel title="RigDesk PCs">
      <Stack gap="sm">
        {pcs.length === 0 ? <Text text='No RigDesk PC is set up. Add one under plugins.rigdesk.pcs: {"name": {"url": "https://<pc>.<tailnet>.ts.net", "token_file": "...", "client": "..."}}.' /> : null}
        {pcs.map(pc => (
          <Row key={pc.name} glyph={<Mark kind="status" shape="square" label={pc.state} ink={pc.holder === 'blocked' ? 'fault' : INK[pc.state]} fill="open" />}
            title={pc.name} word={pc.state}
            aside={pc.down_since_ms === null ? undefined : `down ${agoOf(pc.down_since_ms, now)}`}
            meta={[pc.client, pc.stock ? `stock RustDesk via ${pc.url}` : pc.url, pc.holder === null ? 'no agent' : HOLDER[pc.holder]].filter(Boolean).join(' · ')}
            preview={pc.owner === null ? (pc.why ?? undefined) : <>owned by <OwnerOf owner={pc.owner} />{pc.owned_here ? '' : ' (joined elsewhere)'}</>}
            foot={pc.stock && links[pc.name] !== undefined && pc.owner === null ? <Text text={`Give this link to an agent (copied; it works once): ${links[pc.name] ?? ''}`} /> : undefined}
            actions={pc.stock ? <StockButtons pc={pc} minted={link => { setLinks(held => ({ ...held, [pc.name]: link })) }} /> : undefined} />
        ))}
        <Text text={`RigDesk is AGPL-3.0; its source is at ${read?.source ?? 'https://github.com/deussalo/rigdesk'}.`} />
      </Stack>
    </Panel>
  )
}
