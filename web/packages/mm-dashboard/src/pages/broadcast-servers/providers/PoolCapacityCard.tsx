import { useState } from 'react';
import type { FleetGpuNodesResponse, FleetProvidersResponse, FleetProviderView, FleetRunnerView } from '../../../types';
import { DataTable } from '../../../components/studio/DataTable';
import { KIND_LABEL, poolFree, runningByProvider, testBootZones } from './model';
import { TestBootDialog } from './TestBootDialog';

interface Props {
  providers: FleetProvidersResponse | null;
  gpu: FleetGpuNodesResponse | null;
  runner: FleetRunnerView | null;
  runnerReporting: boolean;
  /** Today's test-boot allowance; null when it could not be read (the server still enforces it). */
  boots: { per_day: number; left_today: number } | null;
  /** A spin-up was queued: the owner follows the request, since a reload would drop it. */
  onTestBootStarted: (requestId: string) => void;
}

/** Why a provider cannot spin up right now, in words; null when it can. */
function spinUpBlocked(demo: boolean, runnerReporting: boolean, boots: Props['boots'], p: FleetProviderView): string | null {
  if (demo) return 'Demo: spin-up is disabled.';
  if (!runnerReporting) return 'The runner is not reporting; spin-up needs it.';
  if (testBootZones(p).length === 0) return 'No zone with a transcode size: add one in the provider form.';
  if (boots !== null && boots.left_today <= 0) return "Today's test boots are used up.";
  return null;
}

/**
 * Pool capacity across GPU providers: the server's cap against running servers, with a
 * per-provider breakdown. Spin-up rents one GPU for at most 15 minutes through the same
 * TestBootDialog the provider form uses; spin-down (release) stays in Running GPU servers
 * below, where every release asks for a reason.
 */
export function PoolCapacityCard({ providers, gpu, runner, runnerReporting, boots, onTestBootStarted }: Props) {
  const [spinUp, setSpinUp] = useState<FleetProviderView | null>(null);
  if (!providers && !gpu) return null;
  const demo = providers?.demo ?? gpu?.demo ?? false;
  const nodes = gpu?.nodes ?? [];
  const running = nodes.length;
  // The server's pool cap when it is known; otherwise the enabled providers' own caps added up.
  const configured = (providers?.providers ?? []).filter((p) => p.enabled).reduce((n, p) => n + p.max_gpu_nodes, 0);
  const cap = gpu?.max_gpu_nodes ?? configured;
  const free = poolFree(cap, running);
  const perProvider = runningByProvider(nodes);
  const enabled = (providers?.providers ?? []).filter((p) => p.enabled);
  const disabledCount = (providers?.providers.length ?? 0) - enabled.length;

  return (
    <div className="card pool-cap">
      <h3>Pool capacity</h3>
      <p role="status">{running} running of {cap} cap · {free} free</p>
      {boots && <p className="muted">Test boots left today: {boots.left_today} of {boots.per_day}</p>}
      {enabled.length === 0 ? <p>No enabled providers.</p> : (
        <DataTable
          columns={['Provider', 'Kind', 'Cap', 'Running', 'Free', '']}
          emptyTitle="No enabled providers."
        >
          {enabled.map((p) => {
            const runningHere = perProvider[p.id] ?? 0;
            const blocked = spinUpBlocked(demo, runnerReporting, boots, p);
            return (
              <tr key={p.id}>
                <td>{p.label}</td>
                <td>{KIND_LABEL[p.kind]}</td>
                <td>{p.max_gpu_nodes}</td>
                <td>{runningHere}</td>
                <td>{poolFree(p.max_gpu_nodes, runningHere)}</td>
                <td>{!demo && (
                  <button
                    type="button"
                    className="btn btn-sm"
                    disabled={blocked !== null}
                    title={blocked ?? 'Rent one GPU for at most 15 minutes'}
                    onClick={() => setSpinUp(p)}
                  >
                    Spin up
                  </button>
                )}</td>
              </tr>
            );
          })}
        </DataTable>
      )}
      {disabledCount > 0 && <p className="muted">{disabledCount} disabled provider{disabledCount === 1 ? '' : 's'} not counted.</p>}
      <p className="muted">Releases happen in Running GPU servers below; every release needs a reason.</p>
      {spinUp && runner && (
        <TestBootDialog
          provider={spinUp}
          runner={runner}
          boots={boots}
          onClose={() => setSpinUp(null)}
          onStarted={(id) => { setSpinUp(null); onTestBootStarted(id); }}
        />
      )}
    </div>
  );
}
