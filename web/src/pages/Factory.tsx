import { useQuery } from '@tanstack/react-query'
import { PageHeader } from '@/components/PageHeader'
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card'
import { Skeleton } from '@/components/ui/skeleton'
import { api, fmtUsd } from '@/lib/api'
import { ApiDown } from './Runs'

/** Read-only view of the factory the server loaded (edit factory/factory.yaml in the repo). */
export default function FactoryPage() {
  const { data, isLoading, error } = useQuery({ queryKey: ['factory'], queryFn: api.factory, staleTime: Infinity })
  if (error) return <ApiDown error={error} />
  if (isLoading) return <Skeleton className="h-64" />
  if (!data) {
    return (
      <>
        <PageHeader title="Factory" />
        <p className="text-sm text-muted-foreground">
          The server was started without a factory. Pass <code className="font-mono">--factory</code> to{' '}
          <code className="font-mono">wh serve</code>.
        </p>
      </>
    )
  }
  return (
    <>
      <PageHeader
        title={data.name}
        description={
          <>
            version <span className="font-mono">{data.version}</span> · repo <span className="font-mono">{data.repo}</span> ·
            base <span className="font-mono">{data.base_ref}</span> · max {data.max_rework_loops} rework loops. Edit{' '}
            <span className="font-mono">factory/factory.yaml</span> to change it; every edit is a new version.
          </>
        }
      />
      <ol className="grid gap-3">
        {data.stages.map((st, i) => (
          <li key={st.id}>
            <Card className="gap-3">
              <CardHeader>
                <CardTitle className="flex flex-wrap items-center gap-2">
                  <span className="text-muted-foreground tabular-nums">{i + 1}.</span>
                  <span className="font-mono">{st.id}</span>
                  <span className="text-sm font-normal text-muted-foreground">agent {st.agent.name}</span>
                  {st.gate ? (
                    <span className="rounded border border-warn/50 px-1.5 text-xs font-normal text-warn">gate: {st.gate}</span>
                  ) : null}
                  {st.on_reject ? (
                    <span className="rounded border px-1.5 text-xs font-normal text-muted-foreground">
                      on reject → {st.on_reject}
                    </span>
                  ) : null}
                </CardTitle>
                <CardDescription className="tabular-nums">
                  {st.agent.model ?? 'default model'} · budget {fmtUsd(st.agent.budget_usd)} · timeout {st.agent.timeout_min}m ·{' '}
                  {st.agent.max_retries} retries · {st.agent.permission_mode}
                </CardDescription>
              </CardHeader>
              <CardContent className="grid gap-2">
                {st.agent.allowed_tools.length ? (
                  <div className="flex flex-wrap gap-1">
                    {st.agent.allowed_tools.map((t) => (
                      <code key={t} className="rounded bg-muted px-1.5 py-0.5 text-xs">
                        {t}
                      </code>
                    ))}
                  </div>
                ) : null}
                <details className="text-sm">
                  <summary className="cursor-pointer text-muted-foreground">Prompt</summary>
                  <pre className="mt-2 max-h-96 overflow-auto rounded-md bg-muted/60 p-3 text-xs whitespace-pre-wrap">
                    {st.agent.prompt}
                  </pre>
                </details>
              </CardContent>
            </Card>
          </li>
        ))}
      </ol>
      {data.scorers.length ? (
        <>
          <h2 className="mt-8 mb-3 text-base font-semibold">Scorers</h2>
          <div className="grid gap-3">
            {data.scorers.map((sc) => (
              <Card key={sc.id} className="gap-3">
                <CardHeader>
                  <CardTitle className="flex flex-wrap items-center gap-2">
                    <span className="font-mono">{sc.id}</span>
                    <span className="text-sm font-normal text-muted-foreground">grades {sc.target}</span>
                  </CardTitle>
                  <CardDescription className="tabular-nums">
                    {sc.description ? `${sc.description} · ` : ''}
                    {sc.model} · pass ≥ {sc.pass_threshold} · samples {Math.round(sc.sampling_rate * 100)}% · budget{' '}
                    {fmtUsd(sc.budget_usd)}
                  </CardDescription>
                </CardHeader>
                <CardContent>
                  <details className="text-sm">
                    <summary className="cursor-pointer text-muted-foreground">Rubric</summary>
                    <pre className="mt-2 max-h-96 overflow-auto rounded-md bg-muted/60 p-3 text-xs whitespace-pre-wrap">
                      {sc.prompt}
                    </pre>
                  </details>
                </CardContent>
              </Card>
            ))}
          </div>
        </>
      ) : null}
    </>
  )
}
