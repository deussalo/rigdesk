import { useEffect, useState, type ReactNode } from 'react'
import { contribution, Pin, PinItems, REREAD_MS, type ListProps } from '@rig/host'

const server = contribution('rigdesk')
const EVERY_MS = Math.min(REREAD_MS, 5_000)

type Session = { readonly pc: string; readonly state: string; readonly holder: 'human' | 'agent' | 'blocked' | null; readonly since_ms: number | null; readonly joined_ms: number }

const SAID: Readonly<Record<NonNullable<Session['holder']>, string>> = { human: 'the human holds input', agent: 'this agent holds input', blocked: 'blocked by the human' }

export function RigdeskAgentPin({ graph, agent }: ListProps<'agent_pins'>): ReactNode {
  const [session, setSession] = useState<Session | null>(null)
  useEffect(() => {
    let live = true
    const once = async (): Promise<void> => {
      const result = await server('GET', `session?graph=${encodeURIComponent(graph)}&agent=${encodeURIComponent(agent)}`)
      if (live) setSession(result.ok ? (result.value as { readonly session: Session | null }).session : null)
    }
    void once()
    const timer = setInterval(() => { void once() }, EVERY_MS)
    return () => { live = false; clearInterval(timer) }
  }, [graph, agent])
  if (session === null) return null
  const holder = session.holder === null ? 'no holder' : SAID[session.holder]
  return (
    <Pin fold="rigdesk" owner={`${graph}/${agent}`} kind="RigDesk" name={`${session.pc} · ${holder}`} done={0} total={0}>
      <PinItems label="RigDesk" items={[
        { text: `PC ${session.pc}: ${session.state}`, mark: session.state === 'up' ? 'done' : 'blocked' },
        { text: holder, mark: session.holder === 'agent' ? 'doing' : session.holder === 'blocked' ? 'blocked' : 'open' },
      ]} />
    </Pin>
  )
}
