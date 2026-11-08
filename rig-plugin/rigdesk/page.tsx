import { useEffect, useState, type ReactNode } from 'react'
import { agoOf, AppTab, contribution, href, Link, Mark, Panel, REREAD_MS, Refusal, Row, Stack, Text, useNow, type Ink, type Refused } from '@rig/host'

const server = contribution('rigdesk')
const EVERY_MS = Math.min(REREAD_MS, 5_000)

type Owner = { readonly id: string | null; readonly title: string | null; readonly graph: string | null }
type PcRow = {
  readonly name: string
  readonly client: string
  readonly url: string
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

export function RigdeskPage(): ReactNode {
  const read = usePcs()
  const now = useNow()
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
            meta={[pc.client, pc.url, pc.holder === null ? 'no agent' : HOLDER[pc.holder]].filter(Boolean).join(' · ')}
            preview={pc.owner === null ? (pc.why ?? undefined) : <>owned by <OwnerOf owner={pc.owner} />{pc.owned_here ? '' : ' (joined elsewhere)'}</>} />
        ))}
        <Text text={`RigDesk is AGPL-3.0; its source is at ${read?.source ?? 'https://github.com/deussalo/rigdesk'}.`} />
      </Stack>
    </Panel>
  )
}
