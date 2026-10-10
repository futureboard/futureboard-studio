// Dev builds only (`bun run dev`, page opened with `?storageDemo`): a canned
// appliance storage state, so the Storage card can be seen and checked on a
// machine with no storage service. `?storageDemo=fallback` shows the chosen
// drive unplugged, `?storageDemo=busy` a format under way,
// `?storageDemo=format` the format dialog. Production builds
// never call this (store.ts guards it with `import.meta.env.DEV`).

import type { StorageMessage, StorageState, StorageVolume } from './protocol.ts'

const INTERNAL_DIR = '/data/livestage/recordings'

const volume = (v: Partial<StorageVolume> & Pick<StorageVolume, 'id' | 'device' | 'disk'>): StorageVolume => ({
  label: '',
  fs: 'exfat',
  model: null,
  size_bytes: 0,
  free_bytes: null,
  mounted: null,
  mount_path: null,
  supported: true,
  ejected: false,
  ...v,
})

function state(variant: string): StorageState {
  const internal = volume({
    id: 'internal',
    label: 'lsdata',
    device: 'nvme0n1p4',
    disk: 'nvme0n1',
    model: 'WD SN530 256GB',
    size_bytes: 239_000_000_000,
    free_bytes: 201_400_000_000,
    mounted: 'rw',
    mount_path: '/data',
  })
  const stick = volume({
    id: '1A2B-3C4D',
    label: 'SHOW 2026',
    device: 'sda1',
    disk: 'sda',
    model: 'SanDisk Ultra',
    size_bytes: 64_023_257_088,
    free_bytes: 41_250_000_000,
    mounted: 'rw',
    mount_path: '/media/1A2B-3C4D',
  })
  const ssd = volume({
    id: '5E6F-7A8B',
    label: 'BACKUP',
    fs: 'ntfs',
    device: 'sdb1',
    disk: 'sdb',
    model: 'Samsung T7',
    size_bytes: 500_107_862_016,
    free_bytes: 6_200_000_000,
    mounted: 'ro',
    mount_path: '/media/5E6F-7A8B',
  })
  const linux = volume({
    id: '',
    fs: 'btrfs',
    device: 'sdb2',
    disk: 'sdb',
    model: 'Samsung T7',
    size_bytes: 120_000_000_000,
    supported: false,
  })
  const ejected = volume({
    id: '9F00-1234',
    label: 'OLD STICK',
    fs: 'vfat',
    device: 'sdc1',
    disk: 'sdc',
    model: 'Kingston DataTraveler',
    size_bytes: 15_500_000_000,
    ejected: true,
  })
  const disks = [
    { disk: 'nvme0n1', model: 'WD SN530 256GB', size_bytes: 256_060_514_304, removable: false, system: true },
    { disk: 'sda', model: 'SanDisk Ultra', size_bytes: 64_023_257_088, removable: true, system: false },
    { disk: 'sdb', model: 'Samsung T7', size_bytes: 1_000_204_886_016, removable: false, system: false },
    { disk: 'sdc', model: 'Kingston DataTraveler', size_bytes: 15_504_900_000, removable: true, system: false },
  ]
  if (variant === 'fallback') {
    return {
      target: { id: '1A2B-3C4D', label: '1A2B-3C4D', available: false, recordings_dir: INTERNAL_DIR },
      volumes: [internal, ssd, linux, ejected],
      disks: disks.filter((d) => d.disk !== 'sda'),
    }
  }
  return {
    target: {
      id: '1A2B-3C4D',
      label: 'SHOW 2026',
      available: true,
      recordings_dir: '/media/1A2B-3C4D/LiveStage Recordings',
    },
    volumes: [internal, stick, ssd, linux, ejected],
    disks,
  }
}

export function storageDemo(real: StorageMessage): StorageMessage {
  const variant = new URLSearchParams(location.search).get('storageDemo') ?? ''
  const storage = state(variant)
  return {
    ...real,
    available: true,
    reason: null,
    storage,
    busy: variant === 'busy' ? { op: 'format', disk: 'sdc', label: 'LIVESTAGE' } : null,
    folder: storage.target.recordings_dir,
  }
}

/** `?storageDemo=format`: the format dialog, open on the stick. */
export function demoFormatDisk(): string | null {
  return new URLSearchParams(location.search).get('storageDemo') === 'format' ? 'sda' : null
}
