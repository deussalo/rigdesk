import { useEffect, useState, type ReactNode } from 'react'
import { contribution, durationOf, HeldRow, href, Mark, REREAD_MS, useNow, type ListProps } from '@rig/host'

const server = contribution('rigdesk')
const EVERY_MS = Math.min(REREAD_MS, 5_000)

type Session = { readonly pc: string; readonly state: string; readonly holder: 'human' | 'agent' | 'blocked' | null; readonly since_ms: number | null; readonly joined_ms: number }

type Ink = 'live' | 'hold' | 'fault' | 'idle'

type Shown = { readonly word: string; readonly ink: Ink }

/** The link's state, each with its own ink; a link still connecting is not yet down. */
const LINKED: Readonly<Record<string, Shown>> = { connecting: { word: 'connecting', ink: 'hold' }, down: { word: 'down', ink: 'fault' }, missing: { word: 'missing', ink: 'fault' } }

/** An up PC the human has taken is paused for the agent: its calls are refused until the human lets go. */
const HELD: Readonly<Record<NonNullable<Session['holder']>, Shown>> = {
  agent: { word: 'up', ink: 'live' }, human: { word: 'paused · human has input', ink: 'hold' }, blocked: { word: 'paused · blocked by the human', ink: 'hold' },
}

/** The PC's link comes first: a PC that is not up says so, whoever holds its input. */
const shownOf = (session: Session): Shown =>
  session.state !== 'up' ? LINKED[session.state] ?? { word: session.state, ink: 'fault' } : session.holder === null ? { word: 'up', ink: 'live' } : HELD[session.holder]

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

/** The RigDesk PC the agent has joined, as one row of its tools beside its shell: the PC, its link up, connecting, down or paused, how long it has been joined; the row opens RigDesk. */
export function RigdeskTool({ graph, agent }: ListProps<'agent_tools'>): ReactNode {
  const session = useSession(graph, agent)
  const now = useNow(1000)
  if (session === null) return null
  const shown = shownOf(session)
  return (
    <HeldRow mark={<Mark kind="icon" icon="desktop" label="RigDesk PC" ink={shown.ink} />} text={`${session.pc} · ${shown.word}`}
      time={durationOf(now - session.joined_ms, 1000)} href={href('plugin', { plugin: 'rigdesk', path: 'all' })} action={null} />
  )
}
