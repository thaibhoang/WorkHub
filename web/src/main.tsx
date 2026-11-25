import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { BrowserRouter } from 'react-router'
import { AuthGate } from '@/components/AuthGate'
import { Toaster } from '@/components/ui/sonner'
import { TooltipProvider } from '@/components/ui/tooltip'
import { LiveProvider } from '@/lib/live'
import App from './App'
import './index.css'

const queryClient = new QueryClient({
  defaultOptions: {
    // SSE drives freshness; the interval is only a safety net.
    queries: { staleTime: 5_000, refetchInterval: 30_000, retry: 1 },
  },
})

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <QueryClientProvider client={queryClient}>
      <AuthGate>
        <LiveProvider>
          <BrowserRouter>
            <TooltipProvider>
              <App />
            </TooltipProvider>
          </BrowserRouter>
        </LiveProvider>
      </AuthGate>
      <Toaster richColors />
    </QueryClientProvider>
  </StrictMode>,
)
