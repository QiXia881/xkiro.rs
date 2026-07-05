const ERROR_CODE_LABELS: Record<string, string> = {
  call_failed: '调用失败',
  too_many_failures: '连续失败达上限，已禁用',
  rate_limited: '被限流',
  quota_exceeded: '额度已用尽，已禁用',
  refresh_failed: '令牌刷新失败',
  too_many_refresh_failures: '令牌连续刷新失败，已禁用',
  invalid_refresh_token: 'refreshToken 失效，已禁用',
  authentication_failed: '认证失败，已禁用',
  credential_suspended: '被上游暂停，已禁用',
  insufficient_balance: '余额不足，已禁用',
  model_unavailable: '模型暂时不可用，已禁用',
  invalid_config: '配置无效，已禁用',
}

export function errorCodeLabel(code: string | null | undefined): string {
  if (!code) return '调用失败'
  return ERROR_CODE_LABELS[code] ?? code
}
