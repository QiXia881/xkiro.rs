import { adminApi as api } from '@/api/client'
import {
  ADMIN_API_ROUTES,
  type ApiRecord,
  asRecord,
  payloadRecord,
  stringField,
  numberField,
  responseKeys,
  credentialDetailsRecord,
  normalizeLoginDetails,
} from './_normalizers'
import type {
  CredentialLoginDetails,
  CredentialLoginDetailsEnvelope,
  OperationSuccessResponse,
  SsoTokenImportResponse,
  StartSocialLoginRequest,
  StartSocialLoginResponse,
  PollSocialLoginResponse,
  StartIdcLoginRequest,
  StartIamSsoLoginResponse,
  CompleteIamSsoLoginResponse,
  StartBuilderIdLoginRequest,
  StartBuilderIdLoginResponse,
  PollBuilderIdLoginResponse,
  StartKiroSsoLoginResponse,
  PollKiroSsoLoginResponse,
  CompleteKiroSsoLoginResponse,
} from '@/types/api'

function normalizeLoginDetailsEnvelope(raw: ApiRecord): CredentialLoginDetailsEnvelope {
  const details = normalizeLoginDetails(credentialDetailsRecord(raw))
  return {
    details,
  }
}

function normalizeLoginSuccessFields(raw: ApiRecord): {
  credentialId?: number
  authMethod?: string
  provider?: string
  details?: CredentialLoginDetails
} {
  const { details } = normalizeLoginDetailsEnvelope(raw)
  return {
    credentialId: details?.id ?? numberField(raw, 'credentialId', 'credential_id'),
    authMethod: (details?.authMethod ?? stringField(raw, 'authMethod', 'auth_method')) || undefined,
    provider: (details?.provider ?? stringField(raw, 'provider')) || undefined,
    details,
  }
}

function normalizeCompletedLoginDetailsEnvelope(raw: ApiRecord): CredentialLoginDetailsEnvelope & {
  success: boolean
} {
  return {
    success: Boolean(raw.success),
    ...normalizeLoginDetailsEnvelope(raw),
  }
}

function normalizeSocialLoginResponse(data: unknown): PollSocialLoginResponse {
  const raw = asRecord(data)
  const status = stringField(raw, 'status')
  if (status === 'success') {
    const login = normalizeLoginSuccessFields(raw)
    const credentialId = login.credentialId
    if (credentialId === undefined) {
      return {
        status: 'error',
        message: `登录成功但响应缺少 credentialId，响应字段: ${responseKeys(raw)}`,
      }
    }
    return {
      status,
      credentialId,
      authMethod: login.authMethod,
      provider: login.provider,
      details: login.details,
    }
  }
  if (status === 'error') {
    return {
      status,
      message: stringField(raw, 'message', 'error') || '登录失败',
    }
  }
  if (status === 'expired') return { status }
  return { status: 'waiting' }
}

export async function startSocialLogin(
  req: StartSocialLoginRequest,
): Promise<StartSocialLoginResponse> {
  const { data } = await api.post<StartSocialLoginResponse>(ADMIN_API_ROUTES.auth.socialStart, req)
  return data
}

export async function pollSocialLogin(
  sessionId: string,
): Promise<PollSocialLoginResponse> {
  const { data } = await api.post(
    ADMIN_API_ROUTES.auth.socialPoll(sessionId),
  )
  return normalizeSocialLoginResponse(data)
}

export async function completeSocialLoginCallback(
  sessionId: string,
  callbackUrl: string,
): Promise<PollSocialLoginResponse> {
  const { data } = await api.post(
    ADMIN_API_ROUTES.auth.socialCallback(sessionId),
    { callbackUrl },
  )
  return normalizeSocialLoginResponse(data)
}

export async function startIamSsoLogin(
  req: StartIdcLoginRequest,
): Promise<StartIamSsoLoginResponse> {
  const { data } = await api.post<StartIamSsoLoginResponse>(
    ADMIN_API_ROUTES.auth.iamSsoStart,
    req,
  )
  const raw = asRecord(data)
  return {
    sessionId: stringField(raw, 'sessionId', 'session_id'),
    authorizeUrl: stringField(raw, 'authorizeUrl', 'authorize_url'),
    expiresIn: numberField(raw, 'expiresIn', 'expires_in') ?? 0,
  }
}

export async function completeIamSsoLogin(
  sessionId: string,
  callbackUrl: string,
): Promise<CompleteIamSsoLoginResponse> {
  const { data } = await api.post<CompleteIamSsoLoginResponse>(
    ADMIN_API_ROUTES.auth.iamSsoComplete,
    { sessionId, callbackUrl },
  )
  return normalizeCompletedLoginDetailsEnvelope(asRecord(data))
}

export async function startKiroSsoLogin(): Promise<StartKiroSsoLoginResponse> {
  const { data } = await api.post<StartKiroSsoLoginResponse>(ADMIN_API_ROUTES.auth.kiroSsoStart, {})
  const raw = asRecord(data)
  return {
    sessionId: stringField(raw, 'sessionId', 'session_id'),
    signInUrl: stringField(raw, 'signInUrl', 'sign_in_url'),
    interval: numberField(raw, 'interval') ?? 2,
  }
}

export async function pollKiroSsoLogin(
  sessionId: string,
): Promise<PollKiroSsoLoginResponse> {
  const { data } = await api.post<PollKiroSsoLoginResponse>(
    ADMIN_API_ROUTES.auth.kiroSsoPoll,
    { sessionId },
  )
  const raw = asRecord(data)
  const { details } = normalizeLoginDetailsEnvelope(raw)
  return {
    success: Boolean(raw.success),
    completed: Boolean(raw.completed),
    status: (stringField(raw, 'status') || undefined) as PollKiroSsoLoginResponse['status'],
    error: stringField(raw, 'error') || undefined,
    details,
  }
}

export async function completeKiroSsoLogin(
  sessionId: string,
  callbackUrl: string,
): Promise<CompleteKiroSsoLoginResponse> {
  const { data } = await api.post<CompleteKiroSsoLoginResponse>(
    ADMIN_API_ROUTES.auth.kiroSsoComplete,
    { sessionId, callbackUrl },
  )
  const raw = asRecord(data)
  return {
    success: Boolean(raw.success),
    status: stringField(raw, 'status') as CompleteKiroSsoLoginResponse['status'],
    redirectUrl: stringField(raw, 'redirectUrl', 'redirect_url') || undefined,
    error: stringField(raw, 'error') || undefined,
  }
}

export async function cancelKiroSsoLogin(sessionId: string): Promise<OperationSuccessResponse> {
  const { data } = await api.post<OperationSuccessResponse>(
    ADMIN_API_ROUTES.auth.kiroSsoCancel,
    { sessionId },
  )
  return data
}

export async function importSsoToken(
  tokens: string[],
  region?: string,
): Promise<SsoTokenImportResponse> {
  const { data } = await api.post(ADMIN_API_ROUTES.auth.ssoToken, {
    token: tokens.join('\n'),
    region: region?.trim() || 'us-east-1',
  })
  const raw = asRecord(data)
  const results = Array.isArray(raw.results) ? raw.results as Array<Record<string, unknown>> : []
  return {
    imported: numberField(raw, 'successCount', 'success_count', 'imported') ?? 0,
    results: results.map((item, index) => ({
      tokenIndex: numberField(item, 'tokenIndex', 'token_index', 'index') ?? index,
      credentialId: numberField(item, 'credentialId', 'credential_id'),
      email: stringField(item, 'email') || undefined,
      error: stringField(item, 'error') || undefined,
    })),
  }
}

// 设备授权登录
export async function startBuilderIdLogin(
  req: StartBuilderIdLoginRequest,
): Promise<StartBuilderIdLoginResponse> {
  const { data } = await api.post<StartBuilderIdLoginResponse>(
    ADMIN_API_ROUTES.auth.builderIdStart,
    req,
  )
  const raw = payloadRecord(data)
  const verificationUriComplete = stringField(raw, 'verificationUriComplete', 'verification_uri_complete')
  const verificationUri = verificationUriComplete || stringField(raw, 'verificationUri', 'verification_uri')
  if (!verificationUri) {
    throw new Error(`后端返回的设备授权验证地址为空，响应字段: ${responseKeys(raw)}`)
  }
  return {
    sessionId: stringField(raw, 'sessionId', 'session_id'),
    userCode: stringField(raw, 'userCode', 'user_code'),
    verificationUri,
    verificationUriComplete: verificationUriComplete || undefined,
    pollInterval: numberField(raw, 'pollInterval', 'poll_interval', 'interval') ?? 5,
    expiresIn: numberField(raw, 'expiresIn', 'expires_in') ?? 0,
  }
}

export async function completeBuilderIdLogin(
  sessionId: string,
  callbackUrl: string,
): Promise<CompleteIamSsoLoginResponse> {
  const { data } = await api.post<CompleteIamSsoLoginResponse>(
    ADMIN_API_ROUTES.auth.builderIdComplete,
    { sessionId, callbackUrl },
  )
  return normalizeCompletedLoginDetailsEnvelope(asRecord(data))
}

export async function pollBuilderIdLogin(sessionId: string): Promise<PollBuilderIdLoginResponse> {
  const { data } = await api.post<PollBuilderIdLoginResponse>(
    ADMIN_API_ROUTES.auth.builderIdPoll,
    { sessionId },
  )
  const raw = asRecord(data)
  if (Boolean(raw.completed)) {
    const login = normalizeLoginSuccessFields(raw)
    return {
      status: 'success',
      credentialId: login.credentialId ?? 0,
      email: login.details?.email,
      authMethod: login.details?.authMethod,
      provider: login.details?.provider,
      details: login.details,
    }
  }
  if (raw.success === false) {
    const status = stringField(raw, 'status')
    if (status === 'expired') return { status }
    return {
      status: 'error',
      message: stringField(raw, 'message', 'error') || '授权失败',
    }
  }
  const status = stringField(raw, 'status')
  if (status === 'success') {
    const login = normalizeLoginSuccessFields(raw)
    return {
      status,
      credentialId: login.credentialId ?? 0,
      email: (login.details?.email ?? stringField(raw, 'email')) || undefined,
      authMethod: login.authMethod,
      provider: login.provider,
      details: login.details,
    }
  }
  if (status === 'error') {
    return {
      status,
      message: stringField(raw, 'message', 'error') || '授权失败',
    }
  }
  if (status === 'expired') return { status }
  return {
    status: 'pending',
    pollInterval: numberField(raw, 'pollInterval', 'poll_interval', 'interval'),
  }
}
