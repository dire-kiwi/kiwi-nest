// The UI's icons, drawn here so the page loads nothing but the server's own files.

import type { ReactNode } from 'react'

function Icon({ children, size = 16 }: { children: ReactNode; size?: number }) {
  return (
    <svg
      className="icon"
      width={size}
      height={size}
      viewBox="0 0 16 16"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.5"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
    >
      {children}
    </svg>
  )
}

/** The mark: a kiwi's egg in a nest. */
export function Logo({ size = 26 }: { size?: number }) {
  return (
    <svg className="logo" width={size} height={size} viewBox="0 0 32 32" aria-hidden="true">
      <ellipse cx="16" cy="12.5" rx="7.2" ry="9.3" fill="#8bbf4a" />
      <ellipse cx="13.4" cy="8.6" rx="1.7" ry="2.6" fill="#fff" opacity="0.35" />
      <path d="M2.5 16.5h27c0 7.2-6 12-13.5 12s-13.5-4.8-13.5-12z" fill="#8a6a3c" />
      <path
        d="M5 19.5c6 2.5 16 2.5 22 0M7.5 23.5c5 2 12 2 17 0M11 17l-2.5 4M21 17l2.5 4M16 21v5"
        fill="none"
        stroke="#c9a66b"
        strokeWidth="1.3"
        strokeLinecap="round"
      />
    </svg>
  )
}

export const IconOverview = () => (
  <Icon>
    <rect x="2" y="2" width="5" height="5" rx="1" />
    <rect x="9" y="2" width="5" height="5" rx="1" />
    <rect x="2" y="9" width="5" height="5" rx="1" />
    <rect x="9" y="9" width="5" height="5" rx="1" />
  </Icon>
)

export const IconApps = () => (
  <Icon>
    <path d="M8 1.8 13.5 4.8v6.4L8 14.2 2.5 11.2V4.8z" />
    <path d="M2.7 4.9 8 8l5.3-3.1M8 8v6" />
  </Icon>
)

export const IconNodes = () => (
  <Icon>
    <rect x="2" y="2.5" width="12" height="4.5" rx="1" />
    <rect x="2" y="9" width="12" height="4.5" rx="1" />
    <path d="M4.5 4.75h.01M4.5 11.25h.01" />
  </Icon>
)

export const IconNests = () => (
  <Icon>
    <path d="M1.8 7.5h12.4c0 3.6-2.8 6-6.2 6s-6.2-2.4-6.2-6z" />
    <ellipse cx="8" cy="5" rx="2.2" ry="2.7" />
  </Icon>
)

export const IconVolumes = () => (
  <Icon>
    <ellipse cx="8" cy="4" rx="5.5" ry="2.2" />
    <path d="M2.5 4v8c0 1.2 2.5 2.2 5.5 2.2s5.5-1 5.5-2.2V4M2.5 8c0 1.2 2.5 2.2 5.5 2.2s5.5-1 5.5-2.2" />
  </Icon>
)

export const IconBackups = () => (
  <Icon>
    <path d="M2.2 8a5.8 5.8 0 1 0 1.7-4.1L2.2 5.6" />
    <path d="M2.2 2.5v3.1h3.1M8 5v3.2l2 1.3" />
  </Icon>
)

export const IconExternal = () => (
  <Icon size={12}>
    <path d="M6 3H3.5v9.5H13V10M9 2.5h4.5V7M13.3 2.7 7.5 8.5" />
  </Icon>
)

export const IconSearch = () => (
  <Icon>
    <circle cx="7" cy="7" r="4.5" />
    <path d="m10.5 10.5 3.5 3.5" />
  </Icon>
)

export const IconLogout = () => (
  <Icon>
    <path d="M6 2.5H3.5v11H6M10.5 5 13.5 8l-3 3M13.5 8H6.5" />
  </Icon>
)

export const IconSun = () => (
  <Icon>
    <circle cx="8" cy="8" r="2.8" />
    <path d="M8 1.5v1.5M8 13v1.5M1.5 8H3M13 8h1.5M3.4 3.4l1.1 1.1M11.5 11.5l1.1 1.1M3.4 12.6l1.1-1.1M11.5 4.5l1.1-1.1" />
  </Icon>
)

export const IconMoon = () => (
  <Icon>
    <path d="M13.5 9.3A5.8 5.8 0 0 1 6.7 2.5a5.8 5.8 0 1 0 6.8 6.8z" />
  </Icon>
)

/** Half light, half dark: the system's theme. */
export const IconAuto = () => (
  <Icon>
    <circle cx="8" cy="8" r="5.8" />
    <path d="M8 2.2v11.6a5.8 5.8 0 0 0 0-11.6z" fill="currentColor" />
  </Icon>
)

export const IconAlert = () => (
  <Icon>
    <path d="M8 2 14.5 13.5h-13z" />
    <path d="M8 6.5v3.2M8 11.7h.01" />
  </Icon>
)

export const IconCheck = () => (
  <Icon>
    <circle cx="8" cy="8" r="6" />
    <path d="m5.3 8.2 1.9 1.9 3.5-3.9" />
  </Icon>
)
