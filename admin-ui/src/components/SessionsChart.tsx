import { useMemo } from 'react';
import type { HourlyBucket } from '@/lib/api';

// Hand-drawn SVG line chart. 24 (or 'hours') buckets across the x axis.
// No external chart lib — keeps the bundle small and the look matches
// the rest of the admin UI.

const W = 720;
const H = 200;
const PAD_L = 36;
const PAD_R = 14;
const PAD_T = 12;
const PAD_B = 26;
const CW = W - PAD_L - PAD_R;
const CH = H - PAD_T - PAD_B;

interface Props {
  data: HourlyBucket[];
  hours: number;
}

export function SessionsChart({ data, hours }: Props) {
  const filled = useMemo(() => {
    // Align to the top of the current hour, then walk back `hours - 1` slots.
    const nowHour = Math.floor(Date.now() / 1000 / 3600) * 3600;
    const start = nowHour - (hours - 1) * 3600;
    const map = new Map(data.map((b) => [b.ts, b.count]));
    const out: HourlyBucket[] = [];
    for (let i = 0; i < hours; i++) {
      const ts = start + i * 3600;
      out.push({ ts, count: map.get(ts) ?? 0 });
    }
    return out;
  }, [data, hours]);

  const dataMax = Math.max(0, ...filled.map((b) => b.count));
  const yTicks = niceTicks(dataMax);
  const yMax = yTicks[yTicks.length - 1] || 1;

  const xOf = (i: number) => PAD_L + (CW * i) / Math.max(1, hours - 1);
  const yOf = (v: number) => PAD_T + CH - (CH * v) / yMax;

  const line = filled.map((b, i) => `${i === 0 ? 'M' : 'L'} ${xOf(i)} ${yOf(b.count)}`).join(' ');
  const area = `${line} L ${xOf(hours - 1)} ${yOf(0)} L ${xOf(0)} ${yOf(0)} Z`;

  return (
    <svg
      viewBox={`0 0 ${W} ${H}`}
      className="w-full h-56 text-zinc-900 dark:text-zinc-100"
      preserveAspectRatio="none"
    >
      {/* y gridlines + labels */}
      {yTicks.map((y, i) => (
        <g key={i}>
          <line
            x1={PAD_L}
            y1={yOf(y)}
            x2={W - PAD_R}
            y2={yOf(y)}
            stroke="currentColor"
            strokeOpacity="0.08"
          />
          <text
            x={PAD_L - 6}
            y={yOf(y)}
            dy="0.32em"
            textAnchor="end"
            fontSize="11"
            fill="currentColor"
            fillOpacity="0.5"
          >
            {y}
          </text>
        </g>
      ))}

      {/* x axis ticks every ~6 buckets + last */}
      {filled.map((b, i) => {
        const stride = Math.max(1, Math.floor(hours / 4));
        if (i % stride !== 0 && i !== filled.length - 1) return null;
        const d = new Date(b.ts * 1000);
        const hh = d.getUTCHours().toString().padStart(2, '0');
        return (
          <text
            key={i}
            x={xOf(i)}
            y={H - 8}
            textAnchor="middle"
            fontSize="11"
            fill="currentColor"
            fillOpacity="0.5"
          >
            {hh}:00
          </text>
        );
      })}

      {/* area fill */}
      <path d={area} fill="currentColor" fillOpacity="0.08" />

      {/* the line itself */}
      <path d={line} fill="none" stroke="currentColor" strokeWidth="2" strokeOpacity="0.85" />

      {/* dots + native title tooltips */}
      {filled.map((b, i) => (
        <g key={i}>
          <circle
            cx={xOf(i)}
            cy={yOf(b.count)}
            r={b.count > 0 ? 3 : 2}
            fill="currentColor"
            fillOpacity={b.count > 0 ? 1 : 0.35}
          />
          <title>{tooltip(b)}</title>
        </g>
      ))}

      {/* axes baseline */}
      <line
        x1={PAD_L}
        y1={PAD_T + CH}
        x2={W - PAD_R}
        y2={PAD_T + CH}
        stroke="currentColor"
        strokeOpacity="0.25"
      />
    </svg>
  );
}

function niceTicks(max: number): number[] {
  if (max < 4) {
    const out: number[] = [];
    for (let v = 0; v <= Math.max(4, max); v++) out.push(v);
    return out;
  }
  const exp = Math.floor(Math.log10(max));
  const pow = Math.pow(10, exp);
  const ratio = max / pow;
  const step = ratio > 5 ? pow * 2 : ratio > 2 ? pow : pow / 2;
  const top = Math.ceil(max / step) * step;
  const ticks: number[] = [];
  for (let v = 0; v <= top + 1e-9; v += step) ticks.push(Math.round(v));
  return ticks;
}

function tooltip(b: HourlyBucket): string {
  const d = new Date(b.ts * 1000);
  const hh = d.toISOString().slice(0, 13).replace('T', ' ');
  return `${hh}:00 UTC — ${b.count} session${b.count === 1 ? '' : 's'}`;
}
