import { useQuery } from '@tanstack/react-query'
import { ClipboardCheck, Factory as FactoryIcon, Gauge, Inbox as InboxIcon, LayoutDashboard, ListTodo, Moon, Sparkles, Sun } from 'lucide-react'
import { lazy, Suspense, useState } from 'react'
import { NavLink, Route, Routes } from 'react-router'
import { Button } from '@/components/ui/button'
import { api } from '@/lib/api'
import { useLive } from '@/lib/live'
import { cn } from '@/lib/utils'
import BenchmarkRun from '@/pages/BenchmarkRun'
import Benchmarks, { BenchmarkCompare } from '@/pages/Benchmarks'
import FactoryPage from '@/pages/Factory'
import Improvement from '@/pages/Improvement'
import Inbox from '@/pages/Inbox'
import ProposalDetail from '@/pages/ProposalDetail'
import RunDetail from '@/pages/RunDetail'
import Runs from '@/pages/Runs'
import Scorers from '@/pages/Scorers'

// Charts (recharts) are the bulk of the bundle; load them only on the dashboard.
const Dashboard = lazy(() => import('@/pages/Dashboard'))

export default function App() {
  return (
    <div className="flex min-h-svh flex-col md:flex-row">
      <Sidebar />
      <main className="min-w-0 flex-1 px-4 py-6 md:px-8">
        <Routes>
          <Route path="/" element={<Runs />} />
          <Route path="/runs/:id" element={<RunDetail />} />
          <Route path="/inbox" element={<Inbox />} />
          <Route path="/dashboard" element={<Suspense><Dashboard /></Suspense>} />
          <Route path="/scorers" element={<Scorers />} />
          <Route path="/improvement" element={<Improvement />} />
          <Route path="/proposals/:id" element={<ProposalDetail />} />
          <Route path="/benchmarks" element={<Benchmarks />} />
          <Route path="/benchmarks/compare" element={<BenchmarkCompare />} />
          <Route path="/benchmarks/:id" element={<BenchmarkRun />} />
          <Route path="/factory" element={<FactoryPage />} />
          <Route path="*" element={<p className="text-muted-foreground">Not found.</p>} />
        </Routes>
      </main>
    </div>
  )
}

function Sidebar() {
  const { data: tasks } = useQuery({ queryKey: ['tasks'], queryFn: () => api.tasks() })
  const { data: proposals } = useQuery({ queryKey: ['proposals', 'inbox'], queryFn: () => api.proposals('inbox') })
  const inbox =
    (tasks?.filter((t) => t.status === 'awaiting_gate' || t.status === 'blocked').length ?? 0) + (proposals?.length ?? 0)
  const items = [
    { to: '/', label: 'Runs', icon: ListTodo, end: true },
    { to: '/inbox', label: 'Inbox', icon: InboxIcon, count: inbox },
    { to: '/dashboard', label: 'Dashboard', icon: LayoutDashboard },
    { to: '/scorers', label: 'Scorers', icon: ClipboardCheck },
    { to: '/improvement', label: 'Improvement', icon: Sparkles },
    { to: '/benchmarks', label: 'Benchmarks', icon: Gauge },
    { to: '/factory', label: 'Factory', icon: FactoryIcon },
  ]
  return (
    <aside className="flex shrink-0 items-center gap-1 border-b bg-sidebar px-3 py-2 md:w-52 md:flex-col md:items-stretch md:border-r md:border-b-0 md:px-3 md:py-5">
      <div className="mr-2 flex items-center gap-2 px-2 font-semibold md:mb-5 md:mr-0">
        <span className="grid size-6 place-items-center rounded-md bg-primary text-[11px] font-bold text-primary-foreground">
          W
        </span>
        <span className="hidden sm:inline">WorkHub</span>
      </div>
      <nav className="flex flex-1 gap-1 overflow-x-auto md:flex-col">
        {items.map(({ to, label, icon: Icon, end, count }) => (
          <NavLink
            key={to}
            to={to}
            end={end}
            className={({ isActive }) =>
              cn(
                'flex items-center gap-2 rounded-md px-2 py-1.5 text-sm whitespace-nowrap text-muted-foreground hover:bg-sidebar-accent hover:text-foreground',
                isActive && 'bg-sidebar-accent font-medium text-foreground',
              )
            }
          >
            <Icon className="size-4" />
            <span className="hidden sm:inline">{label}</span>
            {count ? (
              <span className="ml-auto rounded-full bg-warn/20 px-1.5 text-xs font-medium text-warn tabular-nums">
                {count}
              </span>
            ) : null}
          </NavLink>
        ))}
      </nav>
      <div className="flex items-center gap-1 md:mt-auto md:justify-between md:px-1">
        <LiveDot />
        <ThemeToggle />
      </div>
    </aside>
  )
}

function LiveDot() {
  const { status } = useLive()
  const color = { live: 'bg-ok', connecting: 'bg-warn animate-pulse', offline: 'bg-fail' }[status]
  return (
    <span className="flex items-center gap-1.5 px-1 text-xs text-muted-foreground" title="Live event stream (SSE)">
      <span className={cn('size-2 rounded-full', color)} />
      <span className="hidden md:inline">{status}</span>
    </span>
  )
}

function ThemeToggle() {
  const [dark, setDark] = useState(() => document.documentElement.classList.contains('dark'))
  const toggle = () => {
    const next = !dark
    document.documentElement.classList.toggle('dark', next)
    try {
      localStorage.setItem('wh-theme', next ? 'dark' : 'light')
    } catch {
      /* storage unavailable: theme just won't persist */
    }
    setDark(next)
  }
  return (
    <Button variant="ghost" size="icon-sm" onClick={toggle} aria-label="Toggle theme">
      {dark ? <Sun /> : <Moon />}
    </Button>
  )
}
