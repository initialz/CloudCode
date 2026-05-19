// Tools the webterm UI offers in its open / split dropdowns. The
// agent's agent.toml [tools] map is the actual source of truth at
// runtime — anything not configured there will get a friendly error
// from the hub. Keep this list small and hand-edited for now; later
// we'll pull it dynamically from the agent's Hello frame.
export const KNOWN_TOOLS = ['claude', 'codex'] as const;
export type Tool = (typeof KNOWN_TOOLS)[number];

export const DEFAULT_TOOL: Tool = 'claude';

/** Tools to show for a given agent. If the agent reported a list,
 *  use it (intersected with KNOWN_TOOLS so we only render tools
 *  the SPA knows how to label / pass through). Empty/missing list
 *  = pre-v1.13 agent, fall back to KNOWN_TOOLS. */
export function toolsForAgent(reported: readonly string[] | undefined): Tool[] {
  if (!reported || reported.length === 0) return [...KNOWN_TOOLS];
  return KNOWN_TOOLS.filter((t) => reported.includes(t));
}
