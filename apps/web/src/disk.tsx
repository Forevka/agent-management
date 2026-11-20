import { Badge } from '@/components/ui/badge'
import { Text } from '@/components/fx/layout'
import { formatBytes } from './api'
import type { NodeDisk } from './api'

/** A node's free disk space at a glance: quiet when fine, a warning when low, red when runs are held. */
export function DiskBadge({ disk }: { disk: NodeDisk | null }) {
  if (!disk || disk.least_free === null) {
    return (
      <Text size="1" tone="muted">
        not reported
      </Text>
    )
  }
  const free = `${formatBytes(disk.least_free)} free`
  if (disk.state === 'full') {
    return (
      <Badge tone="danger" dot>
        {free} · runs held
      </Badge>
    )
  }
  if (disk.state === 'low') {
    return (
      <Badge tone="warning" dot>
        {free} · low
      </Badge>
    )
  }
  return (
    <Text size="1" tone="muted">
      {free}
    </Text>
  )
}
