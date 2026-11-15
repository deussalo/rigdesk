import { useEffect, useState, type ReactNode } from 'react'
import { contribution, durationOf, HeldRow, href, Mark, REREAD_MS, useNow, type ListProps } from '@rig/host'

const server = contribution('rigdesk')
const EVERY_MS = Math.min(REREAD_MS, 5_000)

type Session = { readonly pc: string; readonly state: string; readonly holder: 'human' | 'agent' | 'blocked' | null; readonly since_ms: number | null; readonly joined_ms: number }

type Ink = 'live' | 'hold' | 'fault' | 'idle'

const HELD: Readonly<Record<NonNullable<Session['holder']>, { readonly word: string; readonly ink: Ink }>> = {
  agent: { word: 'agent has input', ink: 'live' }, human: { word: 'human has input', ink: 'hold' }, blocked: { word: 'blocked by the human', ink: 'hold' },
}

/** The PC's link comes first: a PC that is not up says so, whoever holds its input. */
const shownOf = (session: Session): { readonly word: string; readonly ink: Ink } =>
  session.state !== 'up' ? { word: session.state, ink: 'fault' } : session.holder === null ? { word: 'joined', ink: 'idle' } : HELD[session.holder]

const useSession = (graph: string, agent: string): Session | null => {
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
  return session
}

/** The RigDesk PC the agent has joined, as one row beside its shell and sub-agents: the PC, where its input stands, how long it has been joined; the row opens RigDesk. */
export function RigdeskHold({ graph, agent }: ListProps<'agent_holds'>): ReactNode {
  const session = useSession(graph, agent)
  const now = useNow(1000)
  if (session === null) return null
  const shown = shownOf(session)
  return (
    <HeldRow mark={<Mark kind="icon" icon="bento" label="RigDesk PC" ink={shown.ink} />} text={`${session.pc} · ${shown.word}`}
      time={durationOf(now - session.joined_ms, 1000)} href={href('plugin', { plugin: 'rigdesk', path: 'all' })} action={null} />
  )
}
