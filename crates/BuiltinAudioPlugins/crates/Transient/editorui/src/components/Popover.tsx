import {
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type ReactNode,
  type RefObject,
} from 'react'
import { createPortal } from 'react-dom'
import { motion } from 'motion/react'

type Props = {
  anchorRef: RefObject<HTMLElement | null>
  onClose: () => void
  align?: 'start' | 'center' | 'end'
  offset?: number
  width?: number
  className?: string
  children: ReactNode
}

const MARGIN = 8

/**
 * Portalled popover anchored to measured trigger bounds and clamped to the
 * editor viewport — same contract as MixStation. Escape and outside-press cancel.
 */
export function Popover({
  anchorRef,
  onClose,
  align = 'end',
  offset = 6,
  width,
  className = '',
  children,
}: Props) {
  const panelRef = useRef<HTMLDivElement | null>(null)
  const [placement, setPlacement] = useState<{
    top: number
    left: number
    origin: string
  } | null>(null)

  useLayoutEffect(() => {
    const place = () => {
      const anchor = anchorRef.current
      const panel = panelRef.current
      if (!anchor || !panel) return

      const bounds = anchor.getBoundingClientRect()
      const panelWidth = width ?? panel.offsetWidth
      const panelHeight = panel.offsetHeight

      let left =
        align === 'start'
          ? bounds.left
          : align === 'center'
            ? bounds.left + bounds.width / 2 - panelWidth / 2
            : bounds.right - panelWidth
      left = Math.min(
        Math.max(MARGIN, left),
        Math.max(MARGIN, window.innerWidth - panelWidth - MARGIN),
      )

      const below = bounds.bottom + offset
      const flip =
        below + panelHeight > window.innerHeight - MARGIN &&
        bounds.top - offset - panelHeight > MARGIN
      setPlacement({
        top: flip ? bounds.top - offset - panelHeight : below,
        left,
        origin: flip ? 'bottom' : 'top',
      })
    }

    place()
    window.addEventListener('resize', place)
    window.addEventListener('scroll', place, true)
    return () => {
      window.removeEventListener('resize', place)
      window.removeEventListener('scroll', place, true)
    }
  }, [anchorRef, align, offset, width])

  useEffect(() => {
    const onPointerDown = (event: MouseEvent) => {
      const target = event.target as Node
      if (panelRef.current?.contains(target) || anchorRef.current?.contains(target)) return
      onClose()
    }
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key !== 'Escape') return
      event.stopPropagation()
      onClose()
    }
    document.addEventListener('mousedown', onPointerDown)
    document.addEventListener('keydown', onKeyDown)
    return () => {
      document.removeEventListener('mousedown', onPointerDown)
      document.removeEventListener('keydown', onKeyDown)
    }
  }, [anchorRef, onClose])

  return createPortal(
    <motion.div
      ref={panelRef}
      initial={{ opacity: 0, scale: 0.97, y: -4 }}
      animate={{ opacity: 1, scale: 1, y: 0 }}
      exit={{ opacity: 0, scale: 0.97, y: -4 }}
      transition={{ duration: 0.14, ease: [0.16, 1, 0.3, 1] }}
      style={{
        position: 'fixed',
        top: placement?.top ?? -9999,
        left: placement?.left ?? -9999,
        width,
        transformOrigin: placement?.origin ?? 'top',
        visibility: placement ? 'visible' : 'hidden',
        zIndex: 1000,
      }}
      className={`overflow-hidden rounded-lg border border-line-hi bg-raised shadow-2xl shadow-black/60 ${className}`}
    >
      {children}
    </motion.div>,
    document.body,
  )
}
