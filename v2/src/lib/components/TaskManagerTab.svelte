<script lang="ts">
  import { onDestroy } from "svelte";
  import { api } from "$lib/api";
  import Icon from "$lib/components/Icon.svelte";
  import { getKeepRunning, setKeepRunning } from "$lib/prefs";
  import {
    blockLabel,
    blockTone,
    canForceStop,
    importanceLabel,
    importanceTip,
    outcomeSummary,
    type OutcomeLine,
  } from "$lib/task-manager";
  import type { StopMode, StopReport, TaskApp, TaskSnapshot } from "$lib/types";

  let {
    serial,
    hardwareId = null,
    active = true,
    onstopped,
    onopenapp,
  }: {
    serial: string;
    hardwareId?: string | null;
    active?: boolean;
    /// A stop changed what is running: other screens' memory figures are stale.
    onstopped?: () => void;
    onopenapp?: (pkg: string) => void;
  } = $props();

  const AUTO_REFRESH_MS = 5000;

  let snapshot = $state<TaskSnapshot | null>(null);
  let loading = $state(false);
  let readError = $state<string | null>(null);
  let lastRead = $state<Date | null>(null);
  let auto = $state(false);
  let visible = $state(true);

  /// Packages with a stop in flight; while any is, reads wait for its result,
  /// which carries a fresh snapshot of its own.
  let stopping = $state<Set<string>>(new Set());
  let stopError = $state<string | null>(null);
  let lastStop = $state<{ lines: OutcomeLine[]; freedMb: number; availableBefore: number | null; availableAfter: number | null } | null>(null);

  let view = $state<"stoppable" | "all">("stoppable");
  let search = $state("");
  let sortBy = $state<"memory" | "name">("memory");
  let expanded = $state<Set<string>>(new Set());
  let keepRunning = $state<Set<string>>(new Set());

  let confirmOpen = $state(false);
  let selection = $state<Set<string>>(new Set());

  let readGeneration = 0;
  let destroyed = false;
  let timer: ReturnType<typeof setTimeout> | null = null;

  const stopBusy = $derived(stopping.size > 0);
  const apps = $derived(snapshot?.apps ?? []);
  const stoppable = $derived(apps.filter((a) => a.verdict.kind === "allowed"));
  const bulkCandidates = $derived(stoppable.filter((a) => !keepRunning.has(a.package)));
  const bulkMb = $derived(round(bulkCandidates.reduce((sum, a) => sum + a.background_mb, 0)));
  const selectedMb = $derived(
    round(bulkCandidates.filter((a) => selection.has(a.package)).reduce((sum, a) => sum + a.background_mb, 0)),
  );
  const otherMb = $derived(round((snapshot?.other ?? []).reduce((sum, p) => sum + p.mb, 0)));

  const rows = $derived.by(() => {
    const needle = search.trim().toLowerCase();
    const base = view === "stoppable" ? stoppable : apps;
    const filtered = needle
      ? base.filter((a) => a.package.toLowerCase().includes(needle) || (a.name ?? "").toLowerCase().includes(needle))
      : base;
    return [...filtered].sort((a, b) =>
      sortBy === "name"
        ? displayName(a).localeCompare(displayName(b))
        : b.total_mb - a.total_mb || a.package.localeCompare(b.package),
    );
  });

  const stopHeadline = $derived.by(() => {
    if (!lastStop) return "";
    const freed = lastStop.freedMb > 0 ? `About ${formatMb(lastStop.freedMb)} freed` : "Nothing was freed";
    const { availableBefore: before, availableAfter: after } = lastStop;
    return before != null && after != null ? `${freed} · free RAM ${formatMb(before)} → ${formatMb(after)}` : freed;
  });

  const ramUsedShare = $derived.by(() => {
    const ram = snapshot?.ram;
    if (ram?.total_mb == null || ram.used_mb == null || ram.total_mb <= 0) return null;
    return Math.min(100, Math.round((ram.used_mb / ram.total_mb) * 100));
  });

  function round(mb: number): number {
    return Math.round(mb * 10) / 10;
  }

  function displayName(app: TaskApp): string {
    return app.name ?? app.package;
  }

  function formatMb(mb: number | null | undefined): string {
    if (mb == null) return "—";
    return mb >= 1024 ? `${(mb / 1024).toFixed(2)} GB` : `${mb.toFixed(mb >= 100 ? 0 : 1)} MB`;
  }

  function clearTimer() {
    if (timer !== null) clearTimeout(timer);
    timer = null;
  }

  function scheduleNext() {
    clearTimer();
    if (auto && active && visible && !destroyed) {
      timer = setTimeout(() => {
        timer = null;
        void read();
      }, AUTO_REFRESH_MS);
    }
  }

  async function read() {
    if (destroyed || loading || stopBusy) return;
    clearTimer();
    const generation = ++readGeneration;
    const requested = serial;
    loading = true;
    readError = null;
    try {
      const next = await api.taskSnapshot(requested);
      if (destroyed || generation !== readGeneration || requested !== serial) return;
      snapshot = next;
      lastRead = new Date();
    } catch (e) {
      if (destroyed || generation !== readGeneration || requested !== serial) return;
      readError = String(e);
      // A TV that cannot answer should not be asked again every few seconds.
      auto = false;
    } finally {
      if (generation === readGeneration) {
        loading = false;
        scheduleNext();
      }
    }
  }

  function toggleAuto() {
    auto = !auto;
    if (auto) void read();
    else clearTimer();
  }

  async function stop(packages: string[], mode: StopMode) {
    if (packages.length === 0 || stopBusy) return;
    // Supersede any read in flight: the stop's own result is fresher.
    readGeneration++;
    loading = false;
    clearTimer();
    const requested = serial;
    const names = new Map(apps.map((a) => [a.package, displayName(a)]));
    stopping = new Set(packages);
    stopError = null;
    try {
      const report: StopReport = await api.stopBackgroundApps(requested, packages, mode);
      if (destroyed || requested !== serial) return;
      const lines = report.results.map((r) => outcomeSummary(r.package, names.get(r.package) ?? r.package, r.outcome, mode));
      lastStop = {
        lines,
        freedMb: round(lines.reduce((sum, l) => sum + l.freedMb, 0)),
        availableBefore: report.ram_before.free_mb,
        availableAfter: report.snapshot?.ram.free_mb ?? null,
      };
      if (report.snapshot) {
        snapshot = report.snapshot;
        lastRead = new Date();
        readError = null;
      } else {
        readError = report.snapshot_error;
      }
      onstopped?.();
    } catch (e) {
      if (destroyed || requested !== serial) return;
      stopError = String(e);
    } finally {
      if (!destroyed) {
        stopping = new Set();
        scheduleNext();
      }
    }
  }

  function toggleExpanded(pkg: string) {
    const next = new Set(expanded);
    if (next.has(pkg)) next.delete(pkg);
    else next.add(pkg);
    expanded = next;
  }

  function toggleKeepRunning(pkg: string) {
    keepRunning = new Set(setKeepRunning(hardwareId, pkg, !keepRunning.has(pkg)));
  }

  function openConfirm() {
    selection = new Set(bulkCandidates.map((a) => a.package));
    confirmOpen = true;
  }

  function toggleSelected(pkg: string) {
    const next = new Set(selection);
    if (next.has(pkg)) next.delete(pkg);
    else next.add(pkg);
    selection = next;
  }

  function confirmStopAll() {
    const packages = bulkCandidates.filter((a) => selection.has(a.package)).map((a) => a.package);
    confirmOpen = false;
    void stop(packages, "kill");
  }

  function onKeydown(e: KeyboardEvent) {
    if (confirmOpen && e.key === "Escape") {
      e.preventDefault();
      confirmOpen = false;
    }
  }

  function visibilityChanged() {
    visible = document.visibilityState !== "hidden";
    if (visible) scheduleNext();
    else clearTimer();
  }

  function forceStopOffer(line: OutcomeLine): TaskApp | null {
    if (!line.offerForceStop) return null;
    const app = apps.find((a) => a.package === line.package);
    return app && canForceStop(app) ? app : null;
  }

  // Keyed by hardware id, which arrives with the device profile and may land
  // after this tab mounts.
  $effect(() => {
    keepRunning = getKeepRunning(hardwareId);
  });

  $effect(() => {
    // Read on open. The tab is mounted only while it is shown.
    if (active && snapshot === null && !loading && readError === null) void read();
    if (!active) clearTimer();
  });

  onDestroy(() => {
    destroyed = true;
    readGeneration++;
    clearTimer();
  });
</script>

<svelte:document onvisibilitychange={visibilityChanged} />
<svelte:window onkeydown={onKeydown} />

<div class="card" role="tabpanel" tabindex={0} id="tabpanel-tasks" aria-labelledby="tab-tasks">
  <div class="card-header">
    <div class="header-title">
      <h2><Icon name="memory" size={20} /> Task Manager</h2>
      <p class="muted small">Running apps, from Android's own memory report</p>
    </div>
    <div class="header-actions">
      <span class="muted small" role="status">
        {#if loading}Reading…{:else if stopBusy}Stopping…{:else if lastRead}Read {lastRead.toLocaleTimeString()}{/if}
      </span>
      <button onclick={() => read()} disabled={loading || stopBusy}>
        <Icon name="refresh" size={14} /> Refresh
      </button>
      <button onclick={toggleAuto} aria-pressed={auto} class:on={auto} disabled={stopBusy && !auto}>
        {auto ? "Auto-refresh on" : "Auto-refresh"}
      </button>
    </div>
  </div>

  {#if readError}
    <p class="error" role="alert">{readError}</p>
  {/if}
  {#if stopError}
    <p class="error" role="alert">{stopError}</p>
  {/if}

  {#if snapshot}
    <div class="summary">
      <div class="tile">
        <span class="tile-label">RAM in use</span>
        <span class="tile-value">
          {formatMb(snapshot.ram.used_mb)}<span class="muted">&nbsp;/ {formatMb(snapshot.ram.total_mb)}</span>
        </span>
        {#if ramUsedShare !== null}
          <span class="meter" aria-hidden="true">
            <span class="meter-fill" class:warn={ramUsedShare >= 85} style:width={`${ramUsedShare}%`}></span>
          </span>
        {/if}
      </div>
      <div class="tile">
        <span class="tile-label">Free</span>
        <span class="tile-value">{formatMb(snapshot.ram.free_mb)}</span>
      </div>
      <div class="tile">
        <span class="tile-label">Running apps</span>
        <span class="tile-value">{apps.length}<span class="muted">&nbsp;· {stoppable.length} stoppable</span></span>
      </div>
      <div class="tile tile-action">
        <span class="tile-label">In background apps</span>
        <span class="tile-value">{formatMb(bulkMb)}</span>
        <button
          class="primary"
          onclick={openConfirm}
          disabled={bulkCandidates.length === 0 || stopBusy}
          aria-label={`Stop all background apps (${bulkCandidates.length})`}
        >
          Stop all{bulkCandidates.length ? ` (${bulkCandidates.length})` : ""}
        </button>
      </div>
    </div>

    {#if lastStop}
      <div class="callout callout-ok result" role="status" aria-live="polite">
        <Icon name="check_circle" size={16} />
        <div class="result-body">
          <strong>{stopHeadline}</strong>
          <ul>
            {#each lastStop.lines as line (line.package)}
              {@const offer = forceStopOffer(line)}
              <li class={`outcome outcome--${line.tone}`} data-package={line.package} data-outcome={line.kind}>
                {line.text}
                {#if offer}
                  <button
                    class="small-action"
                    onclick={() => stop([offer.package], "force_stop")}
                    disabled={stopBusy}
                    title="Also cancels its scheduled jobs and alarms until you open it again. Nothing is disabled."
                  >Force stop</button>
                {/if}
              </li>
            {/each}
          </ul>
        </div>
        <button class="dismiss" onclick={() => (lastStop = null)} title="Dismiss" aria-label="Dismiss">
          <Icon name="close" size={16} />
        </button>
      </div>
    {/if}

    <div class="controls">
      <div class="segmented" role="radiogroup" aria-label="Which apps to show">
        <button role="radio" aria-checked={view === "stoppable"} class:on={view === "stoppable"} onclick={() => (view = "stoppable")}>
          Stoppable ({stoppable.length})
        </button>
        <button role="radio" aria-checked={view === "all"} class:on={view === "all"} onclick={() => (view = "all")}>
          All running apps ({apps.length})
        </button>
      </div>
      <input class="search" type="search" placeholder="Filter by name or package" bind:value={search} aria-label="Filter apps" />
      <label class="sort small">Sort
        <select bind:value={sortBy}>
          <option value="memory">Memory</option>
          <option value="name">Name</option>
        </select>
      </label>
    </div>

    {#if rows.length === 0}
      <p class="muted empty">
        {#if search.trim()}No running app matches “{search.trim()}”.
        {:else if view === "stoppable"}Nothing is running in the background that can be stopped. Android is already keeping this TV tidy.
        {:else}No apps are running.{/if}
      </p>
    {:else}
      <table class="task-table">
        <thead>
          <tr>
            <th class="caret-col"><span class="sr-only">Details</span></th>
            <th>App</th>
            <th>State</th>
            <th class="num">Memory</th>
            <th class="action-col">Action</th>
          </tr>
        </thead>
        <tbody>
          {#each rows as app (app.package)}
            {@const isOpen = expanded.has(app.package)}
            {@const busy = stopping.has(app.package)}
            {@const kept = keepRunning.has(app.package)}
            <tr data-package={app.package} data-verdict={app.verdict.kind === "allowed" ? "allowed" : app.verdict.reason} class:busy>
              <td class="caret-col">
                <button
                  class="caret"
                  onclick={() => toggleExpanded(app.package)}
                  aria-expanded={isOpen}
                  aria-label={`${isOpen ? "Hide" : "Show"} processes of ${displayName(app)}`}
                >
                  <Icon name={isOpen ? "expand_more" : "chevron_right"} size={16} />
                </button>
              </td>
              <td class="app-cell">
                {#if onopenapp}
                  <button class="link" onclick={() => onopenapp?.(app.package)} data-tip="Open in the App List">{displayName(app)}</button>
                {:else}
                  <span>{displayName(app)}</span>
                {/if}
                <span class="pkg mono">{app.package}{app.processes.length > 1 ? ` · ${app.processes.length} processes` : ""}</span>
              </td>
              <td>
                <span class={`chip chip--${app.importance}`} title={importanceTip(app.importance)}>{importanceLabel(app.importance)}</span>
              </td>
              <td class="num mono" class:heavy={app.total_mb >= 200}>
                {formatMb(app.total_mb)}
                {#if app.verdict.kind === "allowed" && app.background_mb < app.total_mb}
                  <span class="sub" title="Only the background part is stopped; the rest is in use.">{formatMb(app.background_mb)} stoppable</span>
                {/if}
              </td>
              <td class="action-col">
                {#if app.verdict.kind === "allowed"}
                  <div class="actions">
                    <button
                      class="keep"
                      class:on={kept}
                      aria-pressed={kept}
                      onclick={() => toggleKeepRunning(app.package)}
                      disabled={!hardwareId}
                      data-tip={hardwareId ? (kept ? "Kept running: Stop all skips it" : "Keep running: Stop all skips it") : "This TV reports no hardware id, so a choice can't be remembered"}
                      aria-label={`Keep ${displayName(app)} running`}
                    >
                      <Icon name="shield" size={14} />
                    </button>
                    <button
                      class="stop"
                      onclick={() => stop([app.package], "kill")}
                      disabled={stopBusy}
                      aria-label={`Stop ${displayName(app)}`}
                    >{busy ? "Stopping…" : "Stop"}</button>
                  </div>
                {:else}
                  <span class={`chip chip--block chip--${blockTone(app.verdict.reason)}`} title={app.verdict.detail}>
                    {blockLabel(app.verdict.reason)}
                  </span>
                {/if}
              </td>
            </tr>
            {#if isOpen}
              <tr class="detail-row">
                <td></td>
                <td colspan="4">
                  {#if app.verdict.kind === "blocked"}
                    <p class="small reason"><Icon name="lock" size={14} /> {app.verdict.detail}</p>
                  {/if}
                  <table class="proc-table">
                    <tbody>
                      {#each app.processes as p, i (`${p.process}:${p.pid ?? i}`)}
                        <tr>
                          <td class="mono">{p.process}</td>
                          <td class="mono muted">{p.pid !== null ? `pid ${p.pid}` : ""}</td>
                          <td><span class={`chip chip--${p.importance}`} title={importanceTip(p.importance)}>{importanceLabel(p.importance)}</span></td>
                          <td class="num mono">{formatMb(p.mb)}</td>
                        </tr>
                      {/each}
                    </tbody>
                  </table>
                </td>
              </tr>
            {/if}
          {/each}
        </tbody>
      </table>
    {/if}

    <details class="others small">
      <summary>System processes not tied to an app ({snapshot.other.length} · {formatMb(otherMb)})</summary>
      <p class="muted">Daemons, hardware services and sandboxes. These are part of Android and are never stopped from here.</p>
      <table class="proc-table">
        <tbody>
          {#each snapshot.other as p, i (`${p.process}:${p.pid ?? i}`)}
            <tr>
              <td class="mono">{p.process}</td>
              <td class="mono muted">{p.pid !== null ? `pid ${p.pid}` : ""}</td>
              <td class="num mono">{formatMb(p.mb)}</td>
            </tr>
          {/each}
        </tbody>
      </table>
    </details>

    <details class="how small">
      <summary>How Stop works, and what it never touches</summary>
      <p>
        Stop sends <span class="mono">am kill</span>, which ends only the processes Android already treats as
        background: running services, the previous app and cached apps. Nothing is disabled or removed, and the app
        opens normally next time. Memory is {snapshot.metric === "pss" ? "PSS" : "RSS (this TV does not report PSS)"},
        so freed amounts are estimates.
      </p>
      <p>
        Never offered: protected and sensitive system packages, system apps that are not reviewed as safe to remove,
        Home screen apps, the active keyboard, enabled accessibility services, and anything on screen, playing or
        otherwise in use. Every stop re-reads the TV and checks again before anything is sent.
      </p>
      <p>
        Some apps restart a service on their own; the result says so and offers Force stop, which also cancels their
        scheduled jobs until you open them. Android reclaims cached apps by itself when it needs memory, so stopping
        matters most for apps that keep services running.
      </p>
      {#if !snapshot.home_handlers_read}
        <p>This TV could not list its Home apps. Known launchers and the current Home app are still left alone.</p>
      {/if}
    </details>
  {:else if loading}
    <p class="muted">Reading running apps…</p>
  {/if}
</div>

{#if confirmOpen}
  <div class="backdrop" role="presentation" onclick={() => (confirmOpen = false)}></div>
  <div class="dialog" role="dialog" aria-modal="true" aria-labelledby="stop-all-title">
    <h2 id="stop-all-title">Stop background apps</h2>
    <p class="muted small">
      Each app's background processes end now and it opens normally next time. Apps you keep running are not listed.
      The TV is checked again before anything is sent.
    </p>
    <ul class="pick">
      {#each bulkCandidates as app (app.package)}
        <li>
          <label>
            <input type="checkbox" checked={selection.has(app.package)} onchange={() => toggleSelected(app.package)} />
            <span>{displayName(app)}</span>
            <span class="muted mono">{formatMb(app.background_mb)}</span>
          </label>
        </li>
      {/each}
    </ul>
    <div class="dialog-actions">
      <span class="muted small">About {formatMb(selectedMb)}</span>
      <span class="spacer"></span>
      <button onclick={() => (confirmOpen = false)}>Cancel</button>
      <button class="primary" onclick={confirmStopAll} disabled={selection.size === 0}>
        Stop {selection.size} app{selection.size === 1 ? "" : "s"}
      </button>
    </div>
  </div>
{/if}

<style>
  .small { font-size: 0.8rem; line-height: 1.5; }
  .header-title p { margin: 0.3rem 0 0; }
  .error { color: var(--danger-text); }
  .sr-only { position: absolute; width: 1px; height: 1px; overflow: hidden; clip: rect(0 0 0 0); white-space: nowrap; }

  .summary {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(11rem, 1fr));
    gap: 0.75rem;
    margin-bottom: 1rem;
  }
  .tile {
    display: flex;
    flex-direction: column;
    gap: 0.3rem;
    padding: 0.8rem 0.9rem;
    background: var(--bg-inset);
    border: 1px solid var(--border);
    border-radius: var(--radius-md);
  }
  .tile-label {
    font-family: var(--mono);
    font-size: 0.68rem;
    letter-spacing: 0.1em;
    text-transform: uppercase;
    color: var(--fg-muted);
  }
  .tile-value { font-size: 1.15rem; font-weight: 650; }
  .tile-value .muted { font-size: 0.85rem; font-weight: 500; }
  .tile-action button { margin-top: 0.2rem; }
  .meter { display: block; height: 4px; border-radius: var(--radius-xs); background: var(--bg-muted); overflow: hidden; }
  .meter-fill { display: block; height: 100%; background: var(--ok); }
  .meter-fill.warn { background: var(--warn); }

  .result { margin-bottom: 1rem; align-items: flex-start; }
  .result-body { flex: 1; }
  .result ul { margin: 0.35rem 0 0; padding-left: 1.1rem; }
  .outcome { margin: 0.15rem 0; }
  .outcome--warn { color: var(--warn); }
  .outcome--bad { color: var(--danger-text); }
  .dismiss { padding: 0.1rem; border: none; background: none; color: var(--fg-muted); }

  .controls { display: flex; flex-wrap: wrap; align-items: center; gap: 0.6rem; margin-bottom: 0.8rem; }
  .segmented { display: inline-flex; border: 1px solid var(--border); border-radius: var(--radius-md); overflow: hidden; }
  .segmented button { border: none; border-radius: 0; background: var(--bg-surface); font-size: 0.82rem; }
  .segmented button.on { background: var(--bg-nav-active); color: var(--fg-primary); }
  .search { flex: 1; min-width: 12rem; }
  .sort { display: inline-flex; align-items: center; gap: 0.4rem; color: var(--fg-muted); }
  button.on { border-color: var(--accent); }
  .empty { padding: 1rem 0; }

  table { width: 100%; border-collapse: collapse; }
  .task-table th {
    text-align: left;
    font-family: var(--mono);
    font-size: 0.68rem;
    letter-spacing: 0.1em;
    text-transform: uppercase;
    color: var(--fg-muted);
    font-weight: 600;
    padding: 0.4rem 0.5rem;
    border-bottom: 1px solid var(--border);
  }
  .task-table > tbody > tr > td { padding: 0.5rem; border-bottom: 1px solid var(--border); vertical-align: middle; }
  .task-table tr.busy { opacity: 0.6; }
  .num { text-align: right; white-space: nowrap; }
  .heavy { color: var(--warn); }
  .sub { display: block; font-size: 0.72rem; color: var(--fg-muted); }
  .caret-col { width: 2rem; }
  .caret { padding: 0.15rem; border: none; background: none; color: var(--fg-muted); }
  .app-cell { display: flex; flex-direction: column; gap: 0.1rem; min-width: 0; }
  .app-cell .link { padding: 0; border: none; background: none; color: var(--fg-primary); font-weight: 550; text-align: left; }
  .app-cell .link:hover { color: var(--accent); background: none; }
  .pkg { font-size: 0.72rem; color: var(--fg-muted); overflow-wrap: anywhere; }
  .action-col { width: 1%; white-space: nowrap; text-align: right; }
  .actions { display: inline-flex; align-items: center; gap: 0.35rem; }
  .keep { padding: 0.3rem 0.4rem; color: var(--fg-muted); }
  .keep.on { color: var(--accent); }
  .stop { min-width: 5.5rem; }

  .chip {
    display: inline-block;
    font-size: 0.72rem;
    font-weight: 600;
    padding: 0.12rem 0.45rem;
    border-radius: var(--radius-sm);
    border: 1px solid var(--border);
    color: var(--fg-secondary);
    background: color-mix(in srgb, var(--fg-primary) 6%, transparent);
    white-space: nowrap;
  }
  .chip--foreground, .chip--visible, .chip--perceptible, .chip--busy {
    color: var(--accent);
    border-color: color-mix(in srgb, var(--accent) 35%, transparent);
    background: var(--accent-surface);
  }
  .chip--service { color: var(--warn); border-color: var(--warn-border); background: var(--warn-surface); }
  .chip--block { font-family: var(--mono); letter-spacing: 0.05em; text-transform: uppercase; }
  .chip--danger { color: var(--danger-text); border-color: var(--danger-border); background: var(--danger-surface); }
  .chip--caution { color: var(--warn); border-color: var(--warn-border); background: var(--warn-surface-2); }

  .detail-row td { padding: 0.2rem 0.5rem 0.8rem; border-bottom: 1px solid var(--border); }
  .reason { display: flex; align-items: flex-start; gap: 0.35rem; margin: 0 0 0.4rem; color: var(--fg-secondary); }
  .proc-table td { padding: 0.25rem 0.5rem; font-size: 0.78rem; border-bottom: 1px dashed var(--border); }
  .proc-table tr:last-child td { border-bottom: none; }

  details { margin-top: 1rem; color: var(--fg-secondary); }
  details summary { cursor: pointer; }
  details p { margin: 0.5rem 0; }
  .others .proc-table { max-width: 40rem; }

  .backdrop { position: fixed; inset: 0; background: var(--scrim); z-index: 10; }
  .dialog {
    position: fixed;
    z-index: 11;
    top: 50%;
    left: 50%;
    transform: translate(-50%, -50%);
    width: min(32rem, calc(100vw - 3rem));
    max-height: calc(100vh - 4rem);
    overflow-y: auto;
    display: flex;
    flex-direction: column;
    gap: 0.6rem;
    padding: 1.25rem;
    background: var(--bg-surface);
    border: 1px solid var(--border);
    border-radius: var(--radius-xl);
    box-shadow: var(--shadow-modal);
  }
  .dialog h2 { margin: 0; font-size: 1.1rem; }
  .dialog p { margin: 0; }
  .pick { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: 0.25rem; }
  .pick label { display: flex; align-items: center; gap: 0.5rem; font-size: 0.88rem; }
  .pick label .mono { margin-left: auto; font-size: 0.78rem; }
  .dialog-actions { display: flex; align-items: center; gap: 0.5rem; padding-top: 0.5rem; border-top: 1px solid var(--border); }
  .spacer { flex: 1; }
  .small-action { margin-left: 0.4rem; padding: 0.1rem 0.5rem; font-size: 0.78rem; }
</style>
