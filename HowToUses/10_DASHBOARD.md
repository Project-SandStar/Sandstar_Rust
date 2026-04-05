# Web Dashboard

## Access
```
http://<host>:8085/
```

## Features

### Summary Cards
Four clickable cards at the top:
- **Total Channels** — click to show all
- **Active (Ok)** — click to filter OK status
- **Faults** — click to filter fault channels
- **Unknown** — click to filter unknown status

### Alarm Panel
Red glowing panel shown when any channel has "fault" or "down" status. Lists alarm channels with ID, name, value, status, type.

### Channel Table
- Sortable columns (click header): ID, Name, Value, Status, Type, Direction
- Searchable (text filter box)
- Filterable by card clicks
- Output channels marked with pencil icon

### History Chart
- Click an **input** channel row to open a time-series chart
- Canvas-based with gradient fill, grid lines, axis labels
- Mouse hover tooltips
- Auto-refreshes every 30 seconds
- Fetches 1 hour of history from `/api/history/{channel}`

### Write Modal
- Click an **output** channel row to open write dialog
- Enter new value and submit
- Writes via `POST /api/pointWrite` at priority 17

### Live Indicators
- Pulsing green dot: connected and receiving data
- Red dot: connection failed
- Data refreshes every 5 seconds via polling

### Keyboard Shortcuts
- `Escape` — close chart/modal
- `/` — focus search box

### Mobile Support
- Responsive at 768px (tablet) and 480px (phone)
- Fixed bottom navigation bar on mobile (Home/Alarms/Sensors/Chart)
- Touch-friendly chart with swipe support
