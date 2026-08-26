import { keepPreviousData, useQuery } from '@tanstack/react-query'
import { getByCredential, getByModel, getOverview, getTimeSeries } from '@/api/stats'
import type { StatsTimeFilter } from '@/types/api'

const COMMON = {
  refetchInterval: 30_000,
  staleTime: 25_000,
  placeholderData: keepPreviousData,
  refetchOnWindowFocus: false,
} as const

export function useOverview() {
  return useQuery({
    queryKey: ['stats', 'overview'],
    queryFn: getOverview,
    ...COMMON,
  })
}

function timeKey(time: StatsTimeFilter) {
  return [
    time.range ?? 'custom',
    time.startDate ?? '',
    time.endDate ?? '',
    time.granularity,
  ] as const
}

export function useTimeSeries(time: StatsTimeFilter) {
  return useQuery({
    queryKey: ['stats', 'timeseries', ...timeKey(time)],
    queryFn: () => getTimeSeries(time),
    ...COMMON,
  })
}

export function useByModel(time: StatsTimeFilter) {
  return useQuery({
    queryKey: ['stats', 'by-model', ...timeKey(time)],
    queryFn: () => getByModel(time),
    ...COMMON,
  })
}

export function useByCredential(time: StatsTimeFilter) {
  return useQuery({
    queryKey: ['stats', 'by-credential', ...timeKey(time)],
    queryFn: () => getByCredential(time),
    ...COMMON,
  })
}
