import { useQuery } from '@tanstack/react-query'
import { ArrowLeft, GitCompare } from 'lucide-react'
import { useState } from 'react'
import { Link, useNavigate, useSearchParams } from 'react-router'
import { BenchComparisonTable, BenchRunsTable, RunBenchmarkDialog, Variant } from '@/components/benchmarks'
import { PageHeader } from '@/components/PageHeader'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card'
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select'
import { Skeleton } from '@/components/ui/skeleton'
import { benchApi, type BenchRun, type BenchSuite } from '@/lib/api'
import { ApiDown } from './Runs'

/** Benchmark suites and runs: measure a factory variant on a fixed task set before rolling it out. */
export default function Benchmarks() {
  const runs = useQuery({
    queryKey: ['benchmarks', 'runs'],
    queryFn: () => benchApi.runs(),
    refetchInterval: (q) => (q.state.data?.some((r) => r.status === 'running' || r.status === 'requested') ? 10_000 : false),
  })
  const suites = useQuery({ queryKey: ['benchmarks', 'suites'], queryFn: benchApi.suites, staleTime: 60_000 })
  if (runs.error) return <ApiDown error={runs.error} />
  const finished = runs.data?.filter((r) => r.status === 'completed') ?? []
  return (
    <>
      <PageHeader
        title="Benchmarks"
        description="A fixed set of tasks, each with a deterministic check, run against one version of the factory. Compare two runs (for example a proposal and its base) before you roll a change out."
        actions={<RunBenchmarkDialog />}
      />
      <h2 className="mb-3 text-base font-semibold">Suites</h2>
      {suites.data ? <Suites suites={suites.data} /> : <Skeleton className="h-24" />}

      {finished.length >= 2 ? <ComparePicker runs={finished} /> : null}

      <h2 className="mt-8 mb-3 text-base font-semibold">Runs</h2>
      {!runs.data ? (
        <Skeleton className="h-40" />
      ) : runs.data.length === 0 ? (
        <p className="rounded-lg border border-dashed p-10 text-center text-sm text-muted-foreground">
          No benchmark runs yet. Use <span className="font-medium">Run benchmark</span>, or{' '}
          <span className="font-mono">wh bench run &lt;suite&gt;</span>. Runs queued here execute in{' '}
          <span className="font-mono">wh serve --foreman</span> or <span className="font-mono">wh bench worker</span>.
        </p>
      ) : (
        <BenchRunsTable rows={runs.data} />
      )}
    </>
  )
}

function Suites({ suites }: { suites: BenchSuite[] }) {
  if (suites.length === 0) {
    return (
      <p className="text-sm text-muted-foreground">
        No suites found. Add YAML files to the server's benchmark directory (<code>--bench-dir</code>, default{' '}
        <code>benchmarks/</code>).
      </p>
    )
  }
  return (
    <div className="grid gap-3 md:grid-cols-2">
      {suites.map((s) => (
        <Card key={s.file} className="gap-2">
          <CardHeader>
            <CardTitle>{s.name ?? s.key}</CardTitle>
            <CardDescription>
              {s.error ? (
                <span className="text-fail">{s.error}</span>
              ) : (
                <>
                  {s.description ? <>{s.description} · </> : null}
                  {s.cases.length} cases · version <code>{s.version}</code>
                </>
              )}
            </CardDescription>
          </CardHeader>
          {s.cases.length ? (
            <CardContent>
              <ul className="grid gap-1 text-sm">
                {s.cases.map((c) => (
                  <li key={c.id} className="flex items-baseline gap-2">
                    <span className="font-mono text-xs">{c.id}</span>
                    <span className="min-w-0 truncate text-muted-foreground">{c.title}</span>
                    <span className="ml-auto shrink-0 text-xs text-muted-foreground">
                      {c.repo}
                      {c.check ? ' · check' : ''}
                    </span>
                  </li>
                ))}
              </ul>
            </CardContent>
          ) : null}
        </Card>
      ))}
    </div>
  )
}

function runLabel(r: BenchRun) {
  return `${r.suite} · ${r.label ? `${r.label} ` : ''}${r.factory_version?.slice(0, 7) ?? ''} · ${r.run_id.slice(-6)}`
}

function ComparePicker({ runs }: { runs: BenchRun[] }) {
  const navigate = useNavigate()
  const [a, setA] = useState(runs[1]?.run_id ?? '')
  const [b, setB] = useState(runs[0]?.run_id ?? '')
  const pick = (value: string, set: (v: string) => void, placeholder: string) => (
    <Select value={value} onValueChange={set}>
      <SelectTrigger className="w-full md:w-80">
        <SelectValue placeholder={placeholder} />
      </SelectTrigger>
      <SelectContent>
        {runs.map((r) => (
          <SelectItem key={r.run_id} value={r.run_id}>
            {runLabel(r)}
          </SelectItem>
        ))}
      </SelectContent>
    </Select>
  )
  return (
    <div className="mt-6 flex flex-wrap items-center gap-2">
      <span className="text-sm text-muted-foreground">Compare</span>
      {pick(a, setA, 'Baseline (A)')}
      <span className="text-sm text-muted-foreground">with</span>
      {pick(b, setB, 'Candidate (B)')}
      <Button variant="outline" disabled={!a || !b || a === b} onClick={() => navigate(`/benchmarks/compare?a=${a}&b=${b}`)}>
        <GitCompare /> Compare
      </Button>
    </div>
  )
}

export function BenchmarkCompare() {
  const [params] = useSearchParams()
  const a = params.get('a') ?? ''
  const b = params.get('b') ?? ''
  const { data, error } = useQuery({
    queryKey: ['benchmarks', 'compare', a, b],
    queryFn: () => benchApi.compare(a, b),
    enabled: !!a && !!b,
  })
  return (
    <>
      <Link to="/benchmarks" className="mb-3 inline-flex items-center gap-1 text-sm text-muted-foreground hover:text-foreground">
        <ArrowLeft className="size-4" /> Benchmarks
      </Link>
      <PageHeader title="Compare benchmark runs" />
      {error ? (
        <ApiDown error={error} />
      ) : !data ? (
        <Skeleton className="h-40" />
      ) : (
        <div className="grid gap-4">
          <div className="grid gap-3 md:grid-cols-2">
            {(['a', 'b'] as const).map((k) => (
              <Card key={k} className="gap-2">
                <CardHeader>
                  <CardTitle>
                    {k.toUpperCase()} ·{' '}
                    <Link to={`/benchmarks/${data[k].run_id}`} className="hover:underline">
                      {data[k].suite}
                    </Link>
                  </CardTitle>
                  <CardDescription>
                    <Variant r={data[k]} />
                  </CardDescription>
                </CardHeader>
              </Card>
            ))}
          </div>
          <BenchComparisonTable c={data} />
        </div>
      )}
    </>
  )
}
