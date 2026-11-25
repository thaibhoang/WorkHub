import { type QueryClient, useQuery, useQueryClient } from '@tanstack/react-query'
import { useEffect, useState, type FormEvent, type ReactNode } from 'react'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import { api, ApiError, type Me, UNAUTHORIZED_EVENT } from '@/lib/api'

/**
 * Renders the app only once the server says who we are. With `wh serve --auth`
 * and no session that means the login form; locally (no auth) it passes straight
 * through. Any 401 later (expired session) drops back to the form.
 */
export function AuthGate({ children }: { children: ReactNode }) {
  const qc = useQueryClient()
  const me = useQuery({ queryKey: ['me'], queryFn: api.me, staleTime: Infinity, refetchInterval: false })

  useEffect(() => {
    const onUnauthorized = () => qc.invalidateQueries({ queryKey: ['me'] })
    window.addEventListener(UNAUTHORIZED_EVENT, onUnauthorized)
    return () => window.removeEventListener(UNAUTHORIZED_EVENT, onUnauthorized)
  }, [qc])

  if (me.isPending) return null
  if (me.isError) {
    return (
      <Centered>
        <p className="text-sm text-muted-foreground">Cannot reach the WorkHub server: {me.error.message}</p>
      </Centered>
    )
  }
  if (me.data.auth_enabled && !me.data.user) return <Login />
  return children
}

/**
 * Drop every cached query of the previous user except `me`, then set `me`.
 * (Not `qc.clear()`: that also removes the `me` query the gate is observing,
 * so the gate would never see the new user and stay on the login form.)
 */
function switchUser(qc: QueryClient, me: Me) {
  qc.removeQueries({ predicate: (q) => q.queryKey[0] !== 'me' })
  qc.setQueryData(['me'], me)
}

function Centered({ children }: { children: ReactNode }) {
  return <div className="grid min-h-svh place-items-center px-4">{children}</div>
}

function Login() {
  const qc = useQueryClient()
  const [username, setUsername] = useState('')
  const [password, setPassword] = useState('')
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)

  const submit = async (e: FormEvent) => {
    e.preventDefault()
    setBusy(true)
    setError(null)
    try {
      const me = await api.login(username.trim(), password)
      switchUser(qc, me)
    } catch (err) {
      setError(err instanceof ApiError ? err.message : 'Login failed')
      setPassword('')
    } finally {
      setBusy(false)
    }
  }

  return (
    <Centered>
      <Card className="w-full max-w-sm">
        <CardHeader>
          <div className="mb-2 grid size-8 place-items-center rounded-md bg-primary text-sm font-bold text-primary-foreground">
            W
          </div>
          <CardTitle>Sign in to WorkHub</CardTitle>
          <CardDescription>Agent factory control room</CardDescription>
        </CardHeader>
        <CardContent>
          <form onSubmit={submit} className="grid gap-4">
            <div className="grid gap-2">
              <Label htmlFor="username">Username</Label>
              <Input
                id="username"
                autoComplete="username"
                autoFocus
                required
                value={username}
                onChange={(e) => setUsername(e.target.value)}
              />
            </div>
            <div className="grid gap-2">
              <Label htmlFor="password">Password</Label>
              <Input
                id="password"
                type="password"
                autoComplete="current-password"
                required
                value={password}
                onChange={(e) => setPassword(e.target.value)}
              />
            </div>
            {error && (
              <p role="alert" className="text-sm text-destructive">
                {error}
              </p>
            )}
            <Button type="submit" disabled={busy}>
              {busy ? 'Signing in…' : 'Sign in'}
            </Button>
          </form>
        </CardContent>
      </Card>
    </Centered>
  )
}

/** Current user + logout, for the sidebar (nothing when auth is off). */
export function UserMenu() {
  const qc = useQueryClient()
  const { data } = useQuery({ queryKey: ['me'], queryFn: api.me, staleTime: Infinity, refetchInterval: false })
  if (!data?.auth_enabled || !data.user) return null
  const logout = async () => {
    await api.logout().catch(() => undefined)
    switchUser(qc, { auth_enabled: true, user: null })
  }
  return (
    <div className="flex items-center gap-1 text-xs text-muted-foreground md:px-1">
      <span className="hidden truncate md:inline" title={`Signed in as ${data.user}`}>
        {data.user}
      </span>
      <Button variant="ghost" size="sm" className="h-7 px-2 text-xs" onClick={logout}>
        Sign out
      </Button>
    </div>
  )
}
