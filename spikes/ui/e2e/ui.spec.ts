// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import { expect, test, type Page } from "@playwright/test";

type ConsoleIssues = {
  readonly warnings: string[];
  readonly errors: string[];
  readonly pageErrors: string[];
};

const TREE_GENERATION_MAX_MS = 2_000;
const TREE_FIRST_VISIBLE_MAX_MS = 5_000;
const DIFF_BUILD_MAX_MS = 10_000;
const EVENT_LOOP_RESPONSE_MAX_MS = 1_000;
const BUTTON_RESPONSE_MAX_MS = 1_000;

/**
 * 记录浏览器真实 console 与 pageerror，是为了把组件生命周期问题从截图误判中分离出来。
 */
function captureConsole(page: Page): ConsoleIssues {
  const issues: ConsoleIssues = { warnings: [], errors: [], pageErrors: [] };
  page.on("console", (message) => {
    if (message.type() === "warning") {
      issues.warnings.push(message.text());
    }
    if (message.type() === "error") {
      issues.errors.push(message.text());
    }
  });
  page.on("pageerror", (error) => issues.pageErrors.push(error.message));
  return issues;
}

/**
 * 检查通用页面身份和首屏内容，是为了确保后续指标确实来自目标探针而不是空壳页面。
 */
async function expectAppShell(page: Page): Promise<void> {
  await expect(page).toHaveTitle("JA UI Spike");
  await expect(page.getByRole("heading", { name: "Harness workbench primitives" })).toBeVisible();
  await expect(page.getByTestId("browser-baseline")).toContainText("Playwright baseline");
}

/**
 * 将 console 问题作为 stop-ship 证据，是为了防止浏览器测试只验证 DOM 而遗漏运行时异常。
 */
function expectNoConsoleIssues(issues: ConsoleIssues): void {
  expect(issues.warnings, `console warnings: ${issues.warnings.join(" | ")}`).toEqual([]);
  expect(issues.errors, `console errors: ${issues.errors.join(" | ")}`).toEqual([]);
  expect(issues.pageErrors, `page errors: ${issues.pageErrors.join(" | ")}`).toEqual([]);
}

/**
 * 通过真实 RAF 与 timer 回调测量主线程是否仍能调度，是为了把“页面没挂死”变成可重复的响应性证据。
 */
async function measureEventLoopResponse(page: Page): Promise<{
  readonly elapsedMs: number;
  readonly rafObserved: boolean;
  readonly timerObserved: boolean;
}> {
  return page.evaluate(
    () =>
      new Promise((resolve) => {
        const started = performance.now();
        window.requestAnimationFrame(() => {
          window.setTimeout(() => {
            resolve({
              elapsedMs: Math.max(1, Math.round(performance.now() - started)),
              rafObserved: true,
              timerObserved: true,
            });
          }, 0);
        });
      }),
  );
}

/**
 * 将浏览器指标写入 Playwright 附件，是为了让主任务能审阅本机实测值而不依赖控制台截取。
 */
async function attachPerformanceEvidence(name: string, evidence: object): Promise<void> {
  await test.info().attach(name, {
    body: JSON.stringify(evidence, null, 2),
    contentType: "application/json",
  });
  console.log(`[performance] ${name} ${JSON.stringify(evidence)}`);
}

test.describe("JA UI component spike", () => {
  test("timeline virtualizes 1000 items and preserves user scroll intent", async ({ page }) => {
    const issues = captureConsole(page);
    await page.goto("/");
    await expectAppShell(page);
    await expect(page.getByTestId("timeline-item-count")).toHaveText("1000");
    const visibleCount = Number(await page.getByTestId("timeline-visible-count").textContent());
    expect(visibleCount).toBeGreaterThan(0);
    expect(visibleCount).toBeLessThan(100);

    await page.getByRole("button", { name: "恢复 snapshot + 回放" }).click();
    await expect(page.getByTestId("timeline-last-seq")).toHaveText("1002");
    await expect(page.getByTestId("timeline-buffered-count")).toHaveText("0");

    const scroll = page.getByTestId("timeline-scroll");
    await scroll.evaluate((element) => {
      element.scrollTop = 0;
      element.dispatchEvent(new Event("scroll"));
    });
    await expect(page.getByTestId("timeline-auto-scroll")).toHaveText("paused");
    await page.getByRole("button", { name: "开始 delta" }).click();
    await page.waitForFunction(
      () => Number(document.querySelector('[data-testid="timeline-last-seq"]')?.textContent) > 1005,
    );
    await expect(page.getByTestId("timeline-auto-scroll")).toHaveText("paused");
    await page.getByRole("button", { name: "停止 delta" }).click();
    await page.getByRole("button", { name: "回到底部" }).click();
    await expect(page.getByTestId("timeline-auto-scroll")).toHaveText("enabled");
    expectNoConsoleIssues(issues);
  });

  test("react-arborist loads 100k nodes while keeping DOM virtualized", async ({ page }) => {
    const issues = captureConsole(page);
    await page.goto("/");
    await expectAppShell(page);
    await page.getByTestId("nav-tree").click();
    await expect(page.getByTestId("file-tree-probe")).toBeVisible();
    await expect(page.getByTestId("tree-mode")).toHaveText("lazy-placeholder");
    await page.getByTestId("load-tree").click();
    await expect(page.getByTestId("tree-mode")).toHaveText("100k-loaded");
    await expect(page.getByTestId("tree-node-count")).toHaveText("101000");
    await page.waitForFunction(
      () => Number(document.querySelector('[data-testid="tree-interaction-ms"]')?.textContent) > 0,
    );
    const treeMetrics = await page.evaluate(() => ({
      generationMs: Number(document.querySelector('[data-testid="tree-load-ms"]')?.textContent),
      firstVisibleMs: Number(document.querySelector('[data-testid="tree-interaction-ms"]')?.textContent),
    }));
    expect(treeMetrics.generationMs).toBeLessThan(TREE_GENERATION_MAX_MS);
    expect(treeMetrics.firstVisibleMs).toBeLessThan(TREE_FIRST_VISIBLE_MAX_MS);
    const eventLoop = await measureEventLoopResponse(page);
    expect(eventLoop.rafObserved).toBe(true);
    expect(eventLoop.timerObserved).toBe(true);
    expect(eventLoop.elapsedMs).toBeLessThan(EVENT_LOOP_RESPONSE_MAX_MS);
    await page.getByRole("button", { name: "读取可见行" }).click();
    await expect(page.getByTestId("tree-refresh-count")).toHaveText("1");
    const renderedRows = Number(await page.getByTestId("tree-rendered-count").textContent());
    expect(renderedRows).toBeGreaterThan(0);
    expect(renderedRows).toBeLessThan(250);
    const buttonStarted = await page.evaluate(() => performance.now());
    await page.getByRole("button", { name: "读取可见行" }).click();
    await expect(page.getByTestId("tree-refresh-count")).toHaveText("2");
    const buttonElapsedMs = await page.evaluate((started) => Math.round(performance.now() - started), buttonStarted);
    expect(buttonElapsedMs).toBeLessThan(BUTTON_RESPONSE_MAX_MS);
    await attachPerformanceEvidence("tree-performance.json", {
      generationMs: treeMetrics.generationMs,
      firstVisibleMs: treeMetrics.firstVisibleMs,
      eventLoopMs: eventLoop.elapsedMs,
      refreshButtonMs: buttonElapsedMs,
      renderedRows,
      thresholds: {
        generationMs: TREE_GENERATION_MAX_MS,
        firstVisibleMs: TREE_FIRST_VISIBLE_MAX_MS,
        eventLoopMs: EVENT_LOOP_RESPONSE_MAX_MS,
        refreshButtonMs: BUTTON_RESPONSE_MAX_MS,
      },
    });

    await page.getByLabel("筛选").fill("component-050");
    await page.waitForFunction(
      () => document.querySelectorAll('[data-testid="file-tree-node"]').length > 0,
    );
    const tree = page.locator('[role="tree"]').first();
    await tree.focus();
    await page.keyboard.press("ArrowDown");
    await page.locator('[data-testid="file-tree-node"] button').first().click();
    await page.locator('[data-testid="file-tree-node"] button').nth(1).click({ modifiers: ["Control"] });
    await expect(page.getByTestId("tree-selected-count")).toHaveText("2");
    expectNoConsoleIssues(issues);
  });

  test("CodeMirror handles a 2MiB readonly merge without React document copies", async ({ page }) => {
    const issues = captureConsole(page);
    await page.goto("/");
    await expectAppShell(page);
    await page.getByTestId("nav-diff").click();
    await expect(page.getByTestId("diff-probe")).toBeVisible();
    await page.getByTestId("load-diff").click();
    await expect(page.getByTestId("diff-loaded")).toHaveText("yes");
    await expect(page.getByTestId("diff-total-bytes")).toHaveText(/^[2-9]\d{6,}$/);
    const diffBytes = Number(await page.getByTestId("diff-total-bytes").textContent());
    await page.waitForFunction(
      () => Number(document.querySelector('[data-testid="diff-chunks"]')?.textContent) > 0,
      undefined,
      { timeout: 120_000 },
    );
    const diffBuildMs = Number(await page.getByTestId("diff-build-ms").textContent());
    expect(diffBuildMs).toBeGreaterThan(0);
    expect(diffBuildMs).toBeLessThan(DIFF_BUILD_MAX_MS);
    const eventLoop = await measureEventLoopResponse(page);
    expect(eventLoop.rafObserved).toBe(true);
    expect(eventLoop.timerObserved).toBe(true);
    expect(eventLoop.elapsedMs).toBeLessThan(EVENT_LOOP_RESPONSE_MAX_MS);
    await expect(page.locator(".cm-editor")).toHaveCount(2);
    await expect(page.locator('.cm-content[contenteditable="false"]')).toHaveCount(2);
    await page.getByTestId("diff-host").evaluate((element) => {
      element.scrollTop = element.scrollHeight;
    });
    const buttonStarted = await page.evaluate(() => performance.now());
    await page.getByRole("button", { name: "卸载编辑器" }).click();
    await expect(page.getByTestId("diff-loaded")).toHaveText("no");
    const buttonElapsedMs = await page.evaluate((started) => Math.round(performance.now() - started), buttonStarted);
    expect(buttonElapsedMs).toBeLessThan(BUTTON_RESPONSE_MAX_MS);
    await expect(page.locator(".cm-editor")).toHaveCount(0);
    await attachPerformanceEvidence("diff-performance.json", {
      bytes: diffBytes,
      buildMs: diffBuildMs,
      eventLoopMs: eventLoop.elapsedMs,
      unloadButtonMs: buttonElapsedMs,
      thresholds: {
        buildMs: DIFF_BUILD_MAX_MS,
        eventLoopMs: EVENT_LOOP_RESPONSE_MAX_MS,
        unloadButtonMs: BUTTON_RESPONSE_MAX_MS,
      },
    });
    expectNoConsoleIssues(issues);
  });

  test("Markdown sanitization rejects executable and leaking markup", async ({ page }) => {
    const issues = captureConsole(page);
    await page.goto("/");
    await expectAppShell(page);
    await page.getByTestId("nav-markdown").click();
    const output = page.getByTestId("markdown-output");
    await expect(output).toContainText("安全渲染样例");
    await expect(output.locator("script, style, iframe, object, embed, form")).toHaveCount(0);
    await expect(output.locator("[onerror], [onclick], [style]")).toHaveCount(0);
    await expect(output.locator('a[href^="javascript:"]')).toHaveCount(0);
    await expect(output.locator("img")).toHaveCount(0);
    const xssMarker = await page.evaluate(() => (window as Window & { __ja_xss?: boolean }).__ja_xss);
    expect(xssMarker).toBeUndefined();
    expectNoConsoleIssues(issues);
  });

  test("xterm disposes surfaces over repeated remounts", async ({ page }) => {
    const issues = captureConsole(page);
    await page.goto("/");
    await expectAppShell(page);
    await page.getByTestId("nav-terminal").click();
    await expect(page.getByTestId("terminal-surface")).toBeVisible();
    await page.setViewportSize({ width: 1024, height: 760 });
    await page.waitForFunction(
      () => Number(document.querySelector('[data-testid="terminal-resize-callbacks"]')?.textContent) > 0,
    );
    for (let cycle = 0; cycle < 5; cycle += 1) {
      await page.getByRole("button", { name: "重新挂载" }).click();
      await expect(page.getByTestId("terminal-surface")).toBeVisible();
    }
    await expect(page.getByTestId("terminal-cycles")).toHaveText("6");
    await expect(page.getByTestId("terminal-active-instances")).toHaveText("1");
    await expect(page.getByTestId("terminal-active-observers")).toHaveText("1");
    await expect(page.getByTestId("terminal-max-instances")).toHaveText("1");
    await expect(page.getByTestId("terminal-max-observers")).toHaveText("1");
    const callbacksBeforeResize = Number(await page.getByTestId("terminal-resize-callbacks").textContent());
    await page.getByTestId("terminal-surface").evaluate((element) => {
      (element as HTMLElement).style.height = "221px";
    });
    await page.waitForFunction(
      (before) => Number(document.querySelector('[data-testid="terminal-resize-callbacks"]')?.textContent) > before,
      callbacksBeforeResize,
    );
    const callbacksAfterResize = Number(await page.getByTestId("terminal-resize-callbacks").textContent());
    expect(callbacksAfterResize - callbacksBeforeResize).toBeLessThanOrEqual(2);
    await page.getByRole("button", { name: "卸载 terminal" }).click();
    await expect(page.getByTestId("terminal-active")).toHaveText("no");
    await expect(page.getByTestId("terminal-dom-count")).toHaveText("0");
    await expect(page.getByTestId("terminal-active-instances")).toHaveText("0");
    await expect(page.getByTestId("terminal-active-observers")).toHaveText("0");
    await attachPerformanceEvidence("terminal-resources.json", {
      cycles: 6,
      activeInstancesAfterRemount: 1,
      activeObserversAfterRemount: 1,
      maxInstances: 1,
      maxObservers: 1,
      singleResizeCallbackDelta: callbacksAfterResize - callbacksBeforeResize,
      activeInstancesAfterUnmount: 0,
      activeObserversAfterUnmount: 0,
    });
    expectNoConsoleIssues(issues);
  });
});
