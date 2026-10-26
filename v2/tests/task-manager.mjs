// Task Manager tab: what it offers, what it sends, and when it stops reading.
//
// The gate itself is Rust (engine/tasks.rs, with unit tests there and a
// simulator test in adb/sim/tests.rs). This pins the UI side of the contract:
// a row the engine blocked never grows a Stop button, a stop names exactly the
// packages the user chose, Keep running is honoured by Stop all, Force stop is
// only offered after an app restarted itself, and auto-refresh ends with the
// tab. Runs in demo mode (VITE_DEMO=1), no device.
import assert from "node:assert/strict";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const V2 = join(dirname(fileURLToPath(import.meta.url)), "..");

async function exercise(browser, base) {
  const page = await browser.newPage({ viewport: { width: 1400, height: 1000 } });
  const errors = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  await page.addInitScript(() => {
    if (!sessionStorage.getItem("task-test-cleared")) {
      localStorage.clear();
      sessionStorage.setItem("task-test-cleared", "1");
    }
    window.__TASK_TEST__ = { calls: [] };
    const timer = setInterval(() => {
      const bridge = window.__TAURI_INTERNALS__;
      if (!bridge?.invoke) return;
      clearInterval(timer);
      const original = bridge.invoke.bind(bridge);
      bridge.invoke = async (command, args = {}) => {
        if (command === "task_snapshot" || command === "stop_background_apps") {
          window.__TASK_TEST__.calls.push({ command, args: JSON.parse(JSON.stringify(args)) });
        }
        return original(command, args);
      };
    }, 0);
  });
  const calls = (command) =>
    page.evaluate((c) => window.__TASK_TEST__.calls.filter((x) => x.command === c), command);

  await page.goto(base, { waitUntil: "networkidle" });
  await page.getByText("NVIDIA SHIELD", { exact: false }).first().click();
  await page.getByRole("tab", { name: "Task Manager", exact: true }).click();
  const tab = page.locator("#tabpanel-tasks");
  await tab.locator("table.task-table tbody tr[data-package]").first().waitFor();
  assert.equal((await calls("task_snapshot")).length, 1, "opening the tab reads once");

  // Stoppable view lists only rows the engine allowed.
  const verdicts = () =>
    tab.locator("table.task-table tbody tr[data-package]").evaluateAll((rows) =>
      rows.map((r) => ({ pkg: r.dataset.package, verdict: r.dataset.verdict, stop: !!r.querySelector("button.stop") })),
    );
  const stoppable = await verdicts();
  assert.ok(stoppable.length > 0);
  assert.ok(stoppable.every((r) => r.verdict === "allowed" && r.stop));

  // In the full list, nothing blocked carries a Stop button, whatever the reason.
  await tab.getByRole("radio", { name: /All running apps/ }).click();
  const all = await verdicts();
  const blocked = all.filter((r) => r.verdict !== "allowed");
  assert.ok(blocked.some((r) => r.verdict === "protected"), "a protected app is listed");
  assert.ok(blocked.some((r) => r.verdict === "launcher"), "the Home app is listed");
  assert.ok(blocked.some((r) => r.verdict === "in_use"), "the on-screen app is listed");
  assert.ok(blocked.every((r) => !r.stop), `blocked rows offer no Stop: ${JSON.stringify(blocked)}`);

  // One Stop sends exactly that package with am kill.
  await tab.getByRole("button", { name: "Stop Disney+", exact: true }).click();
  await tab.locator(".result li[data-package='com.disney.disneyplus']").waitFor();
  let stops = await calls("stop_background_apps");
  assert.deepEqual(stops.at(-1).args.packages, ["com.disney.disneyplus"]);
  assert.equal(stops.at(-1).args.mode, "kill");
  assert.equal(await tab.locator("tr[data-package='com.disney.disneyplus']").count(), 0, "the stopped app leaves the list");

  // An app that restarts itself is reported as such, and only then is Force
  // stop offered; it sends force_stop for that one package.
  assert.equal(await tab.getByRole("button", { name: "Force stop" }).count(), 0);
  await tab.getByRole("button", { name: "Stop Twitch", exact: true }).click();
  const twitchLine = tab.locator(".result li[data-package='tv.twitch.android.app']");
  await twitchLine.waitFor();
  assert.equal(await twitchLine.getAttribute("data-outcome"), "restarted");
  await twitchLine.getByRole("button", { name: "Force stop" }).click();
  await page.waitForFunction(
    () => window.__TASK_TEST__.calls.filter((c) => c.command === "stop_background_apps").length === 3,
  );
  stops = await calls("stop_background_apps");
  assert.deepEqual(stops.at(-1).args, { serial: stops.at(-1).args.serial, packages: ["tv.twitch.android.app"], mode: "force_stop" });

  // Keep running takes an app out of Stop all; unticking another in the
  // confirm drops it too. Nothing outside the confirmed set is sent.
  await tab.getByRole("button", { name: "Keep Plex running" }).click();
  assert.equal(await tab.getByRole("button", { name: "Keep Plex running" }).getAttribute("aria-pressed"), "true");
  await tab.getByRole("button", { name: /^Stop all background apps/ }).click();
  const dialog = page.getByRole("dialog", { name: "Stop background apps" });
  await dialog.waitFor();
  assert.equal(await dialog.getByText("Plex", { exact: true }).count(), 0, "kept app is not offered in Stop all");
  await dialog.getByRole("checkbox", { name: /Netflix/ }).uncheck();
  const offered = await dialog.locator("li").count();
  await dialog.getByRole("button", { name: `Stop ${offered - 1} apps` }).click();
  await page.waitForFunction(
    () => window.__TASK_TEST__.calls.filter((c) => c.command === "stop_background_apps").length === 4,
  );
  stops = await calls("stop_background_apps");
  const bulk = stops.at(-1).args.packages;
  assert.ok(!bulk.includes("com.plexapp.android"), "kept app not sent");
  assert.ok(!bulk.includes("com.netflix.ninja"), "unticked app not sent");
  assert.equal(bulk.length, offered - 1);
  await tab.locator("tr[data-package='com.plexapp.android']").waitFor();

  // The keep choice is per TV and survives a reload.
  await page.reload({ waitUntil: "networkidle" });
  await page.getByRole("tab", { name: "Task Manager", exact: true }).click();
  await tab.getByRole("button", { name: "Keep Plex running" }).waitFor();
  assert.equal(await tab.getByRole("button", { name: "Keep Plex running" }).getAttribute("aria-pressed"), "true");

  // Auto-refresh reads again on its own, and stops when the tab is left.
  const before = (await calls("task_snapshot")).length;
  await tab.getByRole("button", { name: "Auto-refresh" }).click();
  await page.waitForFunction(
    (n) => window.__TASK_TEST__.calls.filter((c) => c.command === "task_snapshot").length >= n + 2,
    before,
    { timeout: 9000 },
  );
  await page.getByRole("tab", { name: "Overview", exact: true }).click();
  const afterLeave = (await calls("task_snapshot")).length;
  await page.waitForTimeout(6000);
  assert.equal((await calls("task_snapshot")).length, afterLeave, "no reads after leaving the tab");

  assert.deepEqual(errors, []);
  await page.close();
  console.log("Task Manager: blocked rows never offer Stop, stops send exactly what was chosen, Keep running, Force stop after restart and auto-refresh lifecycle pass.");
}

const keys = ["VITE_DEMO", "TAURI_DEV_HOST"];
const previous = new Map(keys.map((key) => [key, process.env[key]]));
process.env.VITE_DEMO = "1";
delete process.env.TAURI_DEV_HOST;
let browser, server;
try {
  const { createServer } = await import("vite");
  const { chromium } = await import("playwright");
  server = await createServer({ root: V2, server: { host: "127.0.0.1", port: 0, strictPort: false, hmr: false } });
  await server.listen();
  const address = server.httpServer.address();
  browser = await chromium.launch();
  await exercise(browser, `http://127.0.0.1:${address.port}`);
} finally {
  await browser?.close();
  await server?.close();
  for (const [key, value] of previous) {
    if (value === undefined) delete process.env[key];
    else process.env[key] = value;
  }
}
