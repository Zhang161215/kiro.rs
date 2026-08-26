const API_KEY_STORAGE_KEY = 'adminApiKey'
const HIDE_DISABLED_STORAGE_KEY = 'hideDisabledCredentials'

export const storage = {
  getApiKey: () => localStorage.getItem(API_KEY_STORAGE_KEY),
  setApiKey: (key: string) => localStorage.setItem(API_KEY_STORAGE_KEY, key),
  removeApiKey: () => localStorage.removeItem(API_KEY_STORAGE_KEY),

  getHideDisabled: () => localStorage.getItem(HIDE_DISABLED_STORAGE_KEY) === '1',
  setHideDisabled: (hide: boolean) =>
    localStorage.setItem(HIDE_DISABLED_STORAGE_KEY, hide ? '1' : '0'),
}
