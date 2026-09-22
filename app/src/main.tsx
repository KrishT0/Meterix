import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { getCurrentWindow } from '@tauri-apps/api/window'

import App from './App'
import { TrayPopover } from './TrayPopover'
import './index.css'

const container = document.getElementById('root')
if (!container) throw new Error('#root is missing from index.html')

// One bundle serves two windows. The popover is a fraction of the dashboard, so
// picking the component by window label is cheaper than a second entry point.
const isPopover = getCurrentWindow().label === 'tray'

createRoot(container).render(
  <StrictMode>{isPopover ? <TrayPopover /> : <App />}</StrictMode>,
)
