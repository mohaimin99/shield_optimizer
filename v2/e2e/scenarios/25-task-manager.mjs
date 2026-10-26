// Task Manager: the real engine reading the captured Shield's meminfo. A
// background user app is offered and really stopped; the Home app, protected
// packages and system apps are listed but never offered, and the backend
// refuses them even when asked directly.

import { SHIELD, shieldScenario } from "../lib/worlds.mjs";

const killsOf = (log) =>
  log.map((i) => i.args.join(" ")).filter((cmd) => /am (kill|force-stop) /.test(cmd));

export const scenarios = [
  {
    name: "task-manager-stops-only-what-the-gate-allows",
    async run(ctx) {
      await ctx.reset(shieldScenario());
      await ctx.openDevice(SHIELD.key, "tasks");
      const page = ctx.page;
      const tab = page.locator("#tabpanel-tasks");
      const row = (pkg) => tab.locator(`tr[data-package='${pkg}']`);
      await row("com.plexapp.android").waitFor();
      ctx.assert.equal(await row("com.plexapp.android").getAttribute("data-verdict"), "allowed");
      await ctx.step("stoppable apps from the captured Shield");

      await tab.getByRole("radio", { name: /All running apps/ }).click();
      const expected = {
        "com.spocky.projengmenu": "launcher",
        "com.android.systemui": "protected",
        "com.google.android.inputmethod.latin": "protected",
        "com.google.android.tts": "sensitive",
      };
      for (const [pkg, verdict] of Object.entries(expected)) {
        ctx.assert.equal(await row(pkg).getAttribute("data-verdict"), verdict, pkg);
        ctx.assert.equal(await row(pkg).locator("button.stop").count(), 0, `${pkg} offers no Stop`);
      }
      ctx.assert.equal(await row("com.google.android.gms").locator("button.stop").count(), 0);
      await ctx.step("blocked apps listed with their reason");

      const since = (await ctx.log()).length;
      await tab.getByRole("button", { name: /^Stop Plex/ }).click();
      const line = tab.locator(".result li[data-package='com.plexapp.android']");
      await line.waitFor();
      ctx.assert.equal(await line.getAttribute("data-outcome"), "stopped");
      const sent = killsOf((await ctx.log()).slice(since));
      ctx.assert.equal(sent.length, 1, JSON.stringify(sent));
      ctx.assert.match(sent[0], /am kill com\.plexapp\.android\)/);
      await ctx.waitFor(async () => (await row("com.plexapp.android").count()) === 0, {
        message: "Plex leaves the list after the stop",
      });
      await ctx.step("Plex stopped and gone");

      // The gate is enforced in the backend, not just by hiding buttons.
      const before = (await ctx.log()).length;
      const report = await ctx.invoke("stop_background_apps", {
        serial: SHIELD.key,
        packages: ["com.android.systemui", "com.spocky.projengmenu", "com.google.android.gms"],
        mode: "kill",
      });
      ctx.assert.deepEqual(
        report.value.results.map((r) => r.outcome.kind),
        ["refused", "refused", "refused"],
      );
      ctx.assert.deepEqual(killsOf((await ctx.log()).slice(before)), [], "nothing was sent");

      // A device that rejects the command is reported as a failure, not a stop.
      await ctx.reset(shieldScenario());
      await ctx.fault({
        matches: "am kill",
        effect: { type: "fail", stderr: "java.lang.SecurityException: denied", stdout: "java.lang.SecurityException: denied", exit_code: 255 },
      });
      await ctx.openDevice(SHIELD.key, "tasks");
      await tab.getByRole("button", { name: /^Stop Plex/ }).click();
      await line.waitFor();
      ctx.assert.equal(await line.getAttribute("data-outcome"), "failed");
      await ctx.step("a refused stop is reported as failed");
    },
  },
];
