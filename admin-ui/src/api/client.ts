import axios from 'axios'
import { storage } from '@/lib/storage'

// 共享的 admin API axios 实例
//
// baseURL 指向后端 admin 挂载点；请求拦截器统一注入 x-api-key。
// credentials.ts / proxies.ts 复用同一实例，避免拦截器与 baseURL 重复配置。
export const adminApi = axios.create({
  baseURL: '/api/admin',
  headers: {
    'Content-Type': 'application/json',
  },
})

adminApi.interceptors.request.use(config => {
  const apiKey = storage.getApiKey()
  if (apiKey) {
    config.headers['x-api-key'] = apiKey
  }
  return config
})
