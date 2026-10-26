// Every word the Task Manager tab shows about a running app's state, why it
// cannot be stopped, and what a stop did. The decisions themselves are made
// in Rust (`crates/core/src/engine/tasks.rs`); this only names them.

import type { BlockReason, Importance, KillOutcome, StopMode, TaskApp } from "./types";

const IMPORTANCE: Record<Importance, { label: string; tip: string }> = {
  system: { label: "System", tip: "Part of Android itself." },
  foreground: { label: "On screen", tip: "The app you are using right now." },
  visible: { label: "Visible", tip: "Visible on screen, or working for something that is." },
  perceptible: {
    label: "Active",
    tip: "Doing something you would notice: playing audio, a foreground service, the keyboard.",
  },
  busy: { label: "Busy", tip: "Running a backup or other work Android is protecting." },
  home: { label: "Home", tip: "The Home screen." },
  service: {
    label: "Service",
    tip: "Running a background service. These keep using memory and CPU until stopped.",
  },
  previous: { label: "Previous", tip: "The app you used before this one." },
  cached: {
    label: "Cached",
    tip: "Kept only so it reopens quickly. Android drops cached apps first when it needs memory.",
  },
  unknown: { label: "Unknown", tip: "This TV reported a state we do not recognise." },
};

export function importanceLabel(importance: Importance): string {
  return IMPORTANCE[importance]?.label ?? "Unknown";
}

export function importanceTip(importance: Importance): string {
  return IMPORTANCE[importance]?.tip ?? IMPORTANCE.unknown.tip;
}

const BLOCK: Record<BlockReason, { label: string; tone: "danger" | "caution" | "neutral" }> = {
  protected: { label: "Protected", tone: "danger" },
  sensitive: { label: "Sensitive", tone: "caution" },
  launcher: { label: "Home app", tone: "caution" },
  keyboard: { label: "Keyboard", tone: "caution" },
  accessibility: { label: "Accessibility", tone: "caution" },
  system_app: { label: "System app", tone: "neutral" },
  in_use: { label: "In use", tone: "neutral" },
  unknown_state: { label: "Unknown state", tone: "neutral" },
};

export function blockLabel(reason: BlockReason): string {
  return BLOCK[reason]?.label ?? "Not offered";
}

export function blockTone(reason: BlockReason): "danger" | "caution" | "neutral" {
  return BLOCK[reason]?.tone ?? "neutral";
}

/// Mirrors the engine's `force_stop_verdict`, only to decide whether to show
/// the button. The backend checks again on a fresh read before sending.
export function canForceStop(app: TaskApp): boolean {
  return (
    app.verdict.kind === "allowed" &&
    app.processes.every((p) => p.importance === "service" || p.importance === "previous" || p.importance === "cached")
  );
}

export interface OutcomeLine {
  package: string;
  kind: KillOutcome["kind"];
  text: string;
  tone: "ok" | "warn" | "bad";
  freedMb: number;
  /// The app came back on its own after a plain stop; Force stop may hold it.
  offerForceStop: boolean;
}

function mb(value: number): string {
  return `${value >= 100 ? value.toFixed(0) : value.toFixed(1)} MB`;
}

export function outcomeSummary(pkg: string, name: string, outcome: KillOutcome, mode: StopMode): OutcomeLine {
  const base = { package: pkg, kind: outcome.kind, freedMb: 0, offerForceStop: false };
  const verb = mode === "force_stop" ? "Force stopped" : "Stopped";
  switch (outcome.kind) {
    case "stopped":
      return {
        ...base,
        tone: "ok",
        freedMb: outcome.freed_mb,
        text: `${verb} ${name} · about ${mb(outcome.freed_mb)} freed.`,
      };
    case "restarted":
      return {
        ...base,
        tone: "warn",
        // Net: what it took back by starting again does not count as freed.
        freedMb: Math.max(0, outcome.freed_mb - outcome.now_mb),
        offerForceStop: mode === "kill",
        text: `${verb} ${name}, but it started again on its own (now ${mb(outcome.now_mb)}).`,
      };
    case "kept":
      return {
        ...base,
        tone: "warn",
        freedMb: outcome.freed_mb,
        text: `Android kept part of ${name} running; it may have become active.`,
      };
    case "refused":
      return { ...base, tone: "warn", text: `${name} was not stopped: ${outcome.detail}` };
    case "not_running":
      return { ...base, tone: "ok", text: `${name} had already stopped.` };
    case "failed":
      return { ...base, tone: "bad", text: `${name} could not be stopped: ${outcome.message}` };
    case "unverified":
      return {
        ...base,
        tone: "warn",
        text: `Sent the stop to ${name}, but the TV could not be read again to confirm it. Refresh to check.`,
      };
  }
}
