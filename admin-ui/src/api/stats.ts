import { api } from '@/api/client'
import type {
  CredentialDistribution,
  ModelDistribution,
  OverviewStats,
  StatsTimeFilter,
  TimeSeriesPoint,
} from '@/types/api'

export async function getOverview(): Promise<OverviewStats> {
  const { data } = await api.get<OverviewStats>('/stats/overview')
  return data
}

function statsParams(time: StatsTimeFilter) {
  const params: Record<string, string> = { granularity: time.granularity }
  if (time.startDate && time.endDate) {
    params.startDate = time.startDate
    params.endDate = time.endDate
  } else if (time.range) {
    params.range = time.range
  }
  return params
}

export async function getTimeSeries(time: StatsTimeFilter): Promise<TimeSeriesPoint[]> {
  const { data } = await api.get<TimeSeriesPoint[]>('/stats/timeseries', {
    params: statsParams(time),
  })
  return data
}

export async function getByModel(time: StatsTimeFilter): Promise<ModelDistribution[]> {
  const { data } = await api.get<ModelDistribution[]>('/stats/by-model', {
    params: statsParams(time),
  })
  return data
}

export async function getByCredential(time: StatsTimeFilter): Promise<CredentialDistribution[]> {
  const { data } = await api.get<CredentialDistribution[]>('/stats/by-credential', {
    params: statsParams(time),
  })
  return data
}
