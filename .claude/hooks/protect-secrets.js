// PreToolUse hook (Read | Edit | Write | Grep | NotebookEdit).
//
//  - Credential-bearing tools/ files (the .gitignore block that starts at
//    "Device/site credentials", up to the first blank line): denied, so their
//    passwords never enter the transcript. Deploy and device helpers still run
//    them through Bash; this is a safety net, not a full guard (Bash `cat`
//    bypasses it).
//  - graphify-out/ is generated (`/graphify . --update`): edits are denied.
//
// Reads the hook's JSON from stdin. Anything unexpected lets the call through
// (exit 0, no output): a broken hook must not block normal work.
const fs = require('fs');
const path = require('path');

const deny = reason =>
  process.stdout.write(
    JSON.stringify({
      hookSpecificOutput: {
        hookEventName: 'PreToolUse',
        permissionDecision: 'deny',
        permissionDecisionReason: reason,
      },
    }),
  );

// "/tools/connections/" -> "tools/connections/" (dir) or "tools/x.sh" (file).
function secretEntries(root) {
  const lines = fs.readFileSync(path.join(root, '.gitignore'), 'utf8').split(/\r?\n/);
  const start = lines.findIndex(l => l.includes('Device/site credentials'));
  if (start < 0) return [];
  const entries = [];
  for (const line of lines.slice(start + 1)) {
    if (line.trim() === '') break;
    if (line.startsWith('#')) continue;
    entries.push(line.trim().replace(/^\//, ''));
  }
  return entries;
}

const norm = p => (process.platform === 'win32' ? p.toLowerCase() : p);

let input = '';
process.stdin.on('data', chunk => (input += chunk));
process.stdin.on('end', () => {
  try {
    const event = JSON.parse(input);
    const tool = event?.tool_name;
    const args = event?.tool_input ?? {};
    const target = args.file_path ?? args.notebook_path ?? args.path;
    if (typeof target !== 'string' || target === '') return;

    const root = process.env.CLAUDE_PROJECT_DIR || event?.cwd || process.cwd();
    const relative = path.relative(root, path.resolve(root, target)).replace(/\\/g, '/');
    if (relative.startsWith('..') || path.isAbsolute(relative)) return;
    const rel = norm(relative);

    for (const entry of secretEntries(root)) {
      const e = norm(entry);
      const hit = e.endsWith('/') ? rel === e.slice(0, -1) || rel.startsWith(e) : rel === e;
      if (hit) {
        deny(
          `${relative} holds device credentials (.gitignore credentials block). ` +
            'Do not open it with file tools; device access goes through the Bash helpers, ' +
            'which read credentials at runtime without printing them.',
        );
        return;
      }
    }

    if ((tool === 'Edit' || tool === 'Write') && rel.startsWith('graphify-out/')) {
      deny('graphify-out/ is generated: run `/graphify . --update` instead of editing it.');
    }
  } catch {
    // Let the call through.
  }
});
