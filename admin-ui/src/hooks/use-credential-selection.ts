import { useState } from 'react'

/**
 * 凭据多选状态管理。
 *
 * 从 dashboard 抽出选择用的 Set 状态及其两个辅助函数，
 * 让批量操作 handler 只消费 `selectedIds` / `deselectAll`，
 * 不再关心选择集合本身如何维护。
 */
export function useCredentialSelection() {
  const [selectedIds, setSelectedIds] = useState<Set<number>>(new Set())

  // 选择管理
  const toggleSelect = (id: number) => {
    const newSelected = new Set(selectedIds)
    if (newSelected.has(id)) {
      newSelected.delete(id)
    } else {
      newSelected.add(id)
    }
    setSelectedIds(newSelected)
  }

  const deselectAll = () => {
    setSelectedIds(new Set())
  }

  return { selectedIds, setSelectedIds, toggleSelect, deselectAll }
}
