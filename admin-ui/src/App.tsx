import { useState, useEffect, lazy, Suspense } from 'react'
import { storage } from '@/lib/storage'
import { AUTH_EXPIRED_EVENT } from '@/api/client'
import { LoginPage } from '@/components/login-page'
import { Toaster } from '@/components/ui/sonner'
import { Button } from '@/components/ui/button'
import { HeaderTools } from '@/components/header-tools'
import { Activity, Settings as SettingsIcon, Server, LogOut, Moon, Sun, Globe, Database } from 'lucide-react'

const Dashboard = lazy(() =>
  import('@/components/dashboard').then((m) => ({ default: m.Dashboard })),
)
const OverviewPage = lazy(() =>
  import('@/components/overview-page').then((m) => ({ default: m.OverviewPage })),
)
const SettingsPage = lazy(() =>
  import('@/components/settings-page').then((m) => ({ default: m.SettingsPage })),
)
const ProxyPage = lazy(() =>
  import('@/components/proxy-page').then((m) => ({ default: m.ProxyPage })),
)
const RequestDetailsPanel = lazy(() =>
  import('@/components/request-details-panel').then((m) => ({ default: m.RequestDetailsPanel })),
)

type Tab = 'overview' | 'credentials' | 'proxy' | 'details' | 'settings'

const TABS: { key: Tab; label: string; icon: React.ReactNode }[] = [
  { key: 'overview', label: '概览', icon: <Activity className="h-3.5 w-3.5" /> },
  { key: 'credentials', label: '凭据管理', icon: <Server className="h-3.5 w-3.5" /> },
  { key: 'proxy', label: '代理管理', icon: <Globe className="h-3.5 w-3.5" /> },
  { key: 'details', label: '请求记录', icon: <Database className="h-3.5 w-3.5" /> },
  { key: 'settings', label: '设置', icon: <SettingsIcon className="h-3.5 w-3.5" /> },
]

function readTabFromHash(): Tab {
  const h = window.location.hash.replace(/^#\/?/, '')
  if (h === 'credentials' || h === 'settings' || h === 'overview' || h === 'proxy' || h === 'details') {
    return h
  }
  return 'overview'
}

function App() {
  const [isLoggedIn, setIsLoggedIn] = useState(false)
  const [tab, setTab] = useState<Tab>(readTabFromHash)
  const [darkMode, setDarkMode] = useState(() => {
    if (typeof window !== 'undefined') {
      return document.documentElement.classList.contains('dark')
    }
    return false
  })

  useEffect(() => {
    if (storage.getApiKey()) setIsLoggedIn(true)
  }, [])

  useEffect(() => {
    const onExpired = () => setIsLoggedIn(false)
    window.addEventListener(AUTH_EXPIRED_EVENT, onExpired)
    return () => window.removeEventListener(AUTH_EXPIRED_EVENT, onExpired)
  }, [])

  useEffect(() => {
    const onHash = () => setTab(readTabFromHash())
    window.addEventListener('hashchange', onHash)
    return () => window.removeEventListener('hashchange', onHash)
  }, [])

  const switchTab = (next: Tab) => {
    window.location.hash = `#/${next}`
    setTab(next)
  }

  const handleLogin = () => setIsLoggedIn(true)
  const handleLogout = () => {
    storage.removeApiKey()
    setIsLoggedIn(false)
  }
  const toggleDarkMode = () => {
    setDarkMode((v) => !v)
    document.documentElement.classList.toggle('dark')
  }

  if (!isLoggedIn) {
    return (
      <>
        <LoginPage onLogin={handleLogin} />
        <Toaster position="top-center" />
      </>
    )
  }

  return (
    <>
      <header className="sticky top-0 z-50 w-full glass">
        <div className="mx-auto max-w-[1400px] flex h-12 items-center gap-2 px-3 md:px-6">
          <div className="h-7 w-7 shrink-0 rounded-lg bg-primary/10 text-primary flex items-center justify-center text-xs font-semibold">
            K
          </div>
          <nav className="hidden min-w-0 flex-1 sm:flex items-center gap-0.5 overflow-x-auto rounded-full border border-border/60 p-0.5">
            {TABS.map((t) => (
              <Button
                key={t.key}
                size="sm"
                variant={tab === t.key ? 'default' : 'ghost'}
                className="h-7 shrink-0 rounded-full px-2.5 text-xs"
                onClick={() => switchTab(t.key)}
              >
                {t.icon}
                {t.label}
              </Button>
            ))}
          </nav>
          <div className="ml-auto flex items-center gap-1 shrink-0">
            <HeaderTools />
            <Button
              variant="ghost"
              size="icon"
              className="h-7 w-7"
              onClick={toggleDarkMode}
              title="切换主题"
            >
              {darkMode ? <Sun className="h-4 w-4" /> : <Moon className="h-4 w-4" />}
            </Button>
            <Button
              variant="ghost"
              size="icon"
              className="h-7 w-7"
              onClick={handleLogout}
              title="退出登录"
            >
              <LogOut className="h-4 w-4" />
            </Button>
          </div>
        </div>
        <div className="sm:hidden mx-auto max-w-[1400px] flex items-center gap-1 overflow-x-auto px-3 pb-2">
          {TABS.map((t) => (
            <Button
              key={t.key}
              size="sm"
              variant={tab === t.key ? 'default' : 'ghost'}
              className="h-7 rounded-full px-2.5 text-xs shrink-0"
              onClick={() => switchTab(t.key)}
            >
              {t.icon}
              {t.label}
            </Button>
          ))}
        </div>
      </header>

      <main className="mx-auto max-w-[1400px] px-4 md:px-8 py-6">
        <Suspense
          fallback={
            <div className="text-sm text-muted-foreground">加载中…</div>
          }
        >
          {tab === 'overview' && <OverviewPage />}
          {tab === 'credentials' && <Dashboard onLogout={handleLogout} />}
          {tab === 'proxy' && <ProxyPage />}
          {tab === 'details' && <RequestDetailsPanel />}
          {tab === 'settings' && <SettingsPage />}
        </Suspense>
      </main>

      <Toaster position="top-center" />
    </>
  )
}

export default App
