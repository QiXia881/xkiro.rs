import { adminApi as api } from '@/api/client'
import { ADMIN_API_ROUTES, asRecord, stringField } from './_normalizers'
import type {
  GlobalConfigResponse,
  UpdateGlobalConfigRequest,
  CompressionConfig,
  AccessSettings,
  UpdateAccessSettingsRequest,
  CommonConfig,
  UpdateCommonConfigRequest,
  ThinkingConfig,
  EndpointConfig,
  PromptFilterConfig,
  ProxyConfig,
  UpdateProxyConfigRequest,
  OperationSuccessResponse,
  MessageResponse,
  SystemPromptResponse,
  UpdateSystemPromptRequest,
  UpsertUserPresetRequest,
} from '@/types/api'

// 获取压缩配置
export async function getCompressionConfig(): Promise<CompressionConfig> {
  const { data } = await api.get<CompressionConfig>(ADMIN_API_ROUTES.config.compression)
  return data
}

// 更新压缩配置
export async function setCompressionConfig(config: CompressionConfig): Promise<CompressionConfig> {
  const { data } = await api.put<CompressionConfig>(ADMIN_API_ROUTES.config.compression, config)
  return data
}

// 获取全局配置
export async function getGlobalConfig(): Promise<GlobalConfigResponse> {
  const { data } = await api.get<GlobalConfigResponse>(ADMIN_API_ROUTES.config.global)
  return data
}

// 更新全局配置
export async function updateGlobalConfig(
  req: UpdateGlobalConfigRequest,
): Promise<GlobalConfigResponse> {
  const { data } = await api.put<GlobalConfigResponse>(ADMIN_API_ROUTES.config.global, req)
  return data
}

// ============ 系统提示注入 ============

export async function getSystemPrompt(): Promise<SystemPromptResponse> {
  const { data } = await api.get<SystemPromptResponse>(ADMIN_API_ROUTES.config.systemPrompt)
  return data
}

export async function updateSystemPrompt(
  req: UpdateSystemPromptRequest,
): Promise<SystemPromptResponse> {
  const { data } = await api.put<SystemPromptResponse>(ADMIN_API_ROUTES.config.systemPrompt, req)
  return data
}

export async function upsertUserPreset(
  req: UpsertUserPresetRequest,
): Promise<SystemPromptResponse> {
  const { data } = await api.post<SystemPromptResponse>(ADMIN_API_ROUTES.config.userPresets, req)
  return data
}

export async function deleteUserPreset(id: string): Promise<SystemPromptResponse> {
  const { data } = await api.delete<SystemPromptResponse>(ADMIN_API_ROUTES.config.userPreset(id))
  return data
}

// 代理配置
export async function getProxyConfig(): Promise<ProxyConfig> {
  const { data } = await api.get(ADMIN_API_ROUTES.config.proxy)
  return data
}

export async function updateProxyConfig(req: UpdateProxyConfigRequest): Promise<MessageResponse> {
  const { data } = await api.post(ADMIN_API_ROUTES.config.proxy, req)
  return data
}

export async function getAccessSettings(): Promise<AccessSettings> {
  const { data } = await api.get(ADMIN_API_ROUTES.config.accessSettings)
  return data
}

export async function updateAccessSettings(req: UpdateAccessSettingsRequest): Promise<OperationSuccessResponse> {
  const { data } = await api.post(ADMIN_API_ROUTES.config.accessSettings, req)
  return data
}

export async function getCommonConfig(): Promise<CommonConfig> {
  const { data } = await api.get<CommonConfig>(ADMIN_API_ROUTES.config.common)
  const raw = asRecord(data)
  const strategy = stringField(raw, 'credentialMachineIdStrategy', 'credential_machine_id_strategy')
  return {
    machineId: stringField(raw, 'machineId', 'machine_id'),
    credentialMachineIdStrategy: strategy === 'local' ? 'local' : 'random',
  }
}

export async function updateCommonConfig(req: UpdateCommonConfigRequest): Promise<CommonConfig> {
  const { data } = await api.post<CommonConfig>(ADMIN_API_ROUTES.config.common, req)
  const raw = asRecord(data)
  const strategy = stringField(raw, 'credentialMachineIdStrategy', 'credential_machine_id_strategy')
  return {
    machineId: stringField(raw, 'machineId', 'machine_id'),
    credentialMachineIdStrategy: strategy === 'local' ? 'local' : 'random',
  }
}

export async function getThinkingConfig(): Promise<ThinkingConfig> {
  const { data } = await api.get(ADMIN_API_ROUTES.config.thinking)
  return data
}

export async function updateThinkingConfig(req: ThinkingConfig): Promise<OperationSuccessResponse> {
  const { data } = await api.post(ADMIN_API_ROUTES.config.thinking, req)
  return data
}

export async function getEndpointConfig(): Promise<EndpointConfig> {
  const { data } = await api.get(ADMIN_API_ROUTES.config.endpoint)
  return data
}

export async function updateEndpointConfig(req: EndpointConfig): Promise<OperationSuccessResponse> {
  const { data } = await api.post(ADMIN_API_ROUTES.config.endpoint, req)
  return data
}

export async function getPromptFilterConfig(): Promise<PromptFilterConfig> {
  const { data } = await api.get(ADMIN_API_ROUTES.config.promptFilter)
  return data
}

export async function updatePromptFilterConfig(req: PromptFilterConfig): Promise<OperationSuccessResponse> {
  const { data } = await api.post(ADMIN_API_ROUTES.config.promptFilter, req)
  return data
}
