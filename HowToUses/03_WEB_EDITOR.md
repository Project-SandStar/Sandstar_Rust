# Web-Based Visual DDC Editor

## Access
```
http://<host>:8085/editor
```

## Layout
- **Left sidebar**: Tree navigator (top) + Component palette (bottom)
- **Center canvas**: Visual wire sheet (only user-added components)
- **Right panel**: Property editor (appears on selection)
- **Toolbar**: Hamburger toggle, zoom controls, connection status

## Mouse Controls

| Action | How |
|--------|-----|
| Pan canvas | Left-click drag on empty area |
| Zoom | Scroll wheel |
| Select node | Click on node |
| Multi-select | Ctrl+click nodes, or Ctrl+drag box |
| Move node | Drag node header |
| Move multiple | Select multiple, then drag any |
| Create wire | Drag from output port to input port |
| Select wire | Click on a wire |
| Deselect | Click on empty canvas |
| Context menu | Right-click on node, wire, or canvas |

## Keyboard Shortcuts

| Key | Action |
|-----|--------|
| `/` | Open component palette search |
| `Delete` / `Backspace` | Delete selected node or wire |
| `Ctrl+Z` | Undo |
| `Ctrl+Y` / `Ctrl+Shift+Z` | Redo |
| `Ctrl+A` | Select all components |
| `F` | Fit all nodes in view |
| `L` | Toggle wire labels |
| `A` | Toggle flow animation |
| `Escape` | Close palette / deselect |

## Adding Components

### From Palette (sidebar)
1. Browse categories in the left sidebar bottom section
2. Click a type to add at viewport center
3. Or type in the search box to filter

### Drag and Drop
1. Grab a component type from the palette
2. Drag it onto the canvas
3. Drop at desired position

### From Context Menu
1. Right-click on empty canvas
2. Select "Add Component..."

## Editing Values

1. Click a node to select it
2. The property panel opens on the right
3. **Config/Input slots**: Click the value field, type new value, press Enter
4. **Action slots**: Click the "Invoke" button
5. **Output slots**: Read-only (computed by dataflow engine)

### Value Types
- **Float**: Enter decimal number (e.g., `72.5`)
- **Int**: Enter integer (e.g., `42`)
- **Bool**: Enter `true` or `false`
- **String**: Enter text

## Wiring Components

1. Drag from an **output port** (right side, colored dot)
2. Drop on an **input port** (left side) of another component
3. The wire appears as a bezier curve
4. Colors match data types: green=float, red=bool, teal=int, pink=string

## Naming Rules (Sedona Compatible)
- Maximum 31 characters
- Must start with a letter (a-z, A-Z)
- Only letters, numbers, and underscores allowed
- Double-click node name to rename inline

## Component Categories (43 types)

| Category | Types |
|----------|-------|
| Arithmetic | Add2, Add4, Sub2, Sub4, Mul2, Mul4, Div2 |
| Math | Neg, FloatOffset, Max, Min, Limiter, Round |
| Logic | And2, And4, Or2, Or4, Not, Xor |
| Comparator | Cmpr |
| Conversion | B2P, F2I, I2F |
| Switch | ASW, BSW, ISW |
| Hysteresis | Hysteresis, SRLatch, Reset |
| Constant | ConstFloat, ConstInt, ConstBool |
| Actuator | WriteFloat, WriteBool, WriteInt |
| Stateful | DlyOn, DlyOff, Count, Ramp, Tstat, UpDn |
| Sequencer | LSeq |
| Sensor | ChannelRead |

## Channel Bridge (chXXXX naming)

Rename a ConstFloat to `ch<channelId>` (e.g., `ch1713`) to bridge a live sensor value into logic:
- **Input channels** (AI, DI): Sensor value flows INTO the ConstFloat
- **Output channels** (DO, AO, PWM): ConstFloat value flows TO the hardware
