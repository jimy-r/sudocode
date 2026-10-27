// Paint and interact with the real CLI through the existing browser terminal.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const http = require('node:http');
const shared = path.resolve(__dirname, '../../../../../e2e/terminal-resize');
const moduleRoot = process.env.SCODE_VIEWPORT_BROWSER_MODULES || path.join(shared, 'node_modules');
const modulePath = name => path.join(moduleRoot, name);
const { chromium } = require(modulePath('playwright'));
const { WebSocketServer } = require(modulePath('ws'));
const { launch } = require(path.join(shared, 'host.cjs'));
const sampling = require(path.join(shared, 'performance-policy.json')).sampling;
const config = JSON.parse(fs.readFileSync(process.argv[2], 'utf8'));
const routes = {
  '/': ['text/html', path.join(shared, 'gui.html')],
  '/xterm.js': ['text/javascript', require.resolve(modulePath('@xterm/xterm'))],
  '/xterm.css': ['text/css', path.join(path.dirname(require.resolve(modulePath('@xterm/xterm'))), '../css/xterm.css')],
  '/unicode.js': ['text/javascript', require.resolve(modulePath('@xterm/addon-unicode11'))],
};
const server = http.createServer((request, response) => {
  const route = routes[new URL(request.url, 'http://localhost').pathname];
  if (!route) { response.writeHead(404).end(); return; }
  response.writeHead(200, { 'Content-Type': route[0] }); response.end(fs.readFileSync(route[1]));
});
const websocket = new WebSocketServer({ server });
const report = { status: 'running', enabled: true, samples: [], checks: [],
  measurement: 'Real Chromium mouse/keyboard, terminal raster screenshots and parsed-key timestamps; no physical display scanout measurement.' };
let child, browser, page, exited = false;
const wire = [];
websocket.on('connection', socket => {
  const terminalModules = process.env.SCODE_TERMINAL_MODULES || path.join(shared, 'node_modules');
  child = launch(config, { cols: config.cols, rows: config.rows }, name => require(path.join(terminalModules, name)));
  child.onData(data => { wire.push(data); if (socket.readyState === 1) socket.send(data); });
  child.onExit(() => { exited = true; });
  socket.on('message', raw => {
    const message = JSON.parse(raw);
    if (typeof message.data === 'string') child.write(message.data);
    else child.resize(message.cols, message.rows);
  });
});

(async () => {
  fs.mkdirSync(config.logRoot, { recursive: true });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  try {
    browser = await chromium.launch({ headless: true, executablePath: process.env.SCODE_VIEWPORT_CHROME || undefined });
    const cdp = await browser.newBrowserCDPSession();
    report.browser = await browser.version();
    report.processes_before = (await cdp.send('SystemInfo.getProcessInfo')).processInfo;
    page = await browser.newPage({ viewport: { width: 1300, height: 1000 } });
    await page.goto(`http://127.0.0.1:${server.address().port}/?options=${encodeURIComponent(JSON.stringify({ cols: config.cols, rows: config.rows }))}`);
    await page.waitForFunction(() => window.fixture?.viewport().includes('❯'));
    await page.locator('.xterm-helper-textarea').focus();
    await page.keyboard.insertText(config.prompt);
    await page.keyboard.press('Enter');
    await page.waitForFunction(() => window.fixture.viewport().includes('ViewportRow089'));
    await page.keyboard.insertText('draft界');
    await page.waitForFunction(() => window.fixture.viewport().includes('draft界'));
    const rect = await page.locator('.xterm-screen').boundingBox();
    await page.mouse.move(rect.x + 300, rect.y + 120);
    await page.mouse.wheel(0, -350);
    await page.waitForFunction(() => !window.fixture.viewport().includes('ViewportRow089') && window.fixture.viewport().includes('ViewportRow'));
    const visible = await page.evaluate(() => window.fixture.viewport().split('\n'));
    assert(visible[0].includes('PROSE_CONTROL'), 'context belongs to the answer being read');
    assert(visible.slice(-6).some(row => row.includes('draft界')), 'composer remains fixed');
    report.checks.push('real wheel navigation; pinned prompt and draft');
    await page.screenshot({ path: path.join(config.logRoot, 'reading.png') });

    const label = visible.slice(1).map(row => row.match(/ViewportRow\d+/)?.[0]).find(Boolean);
    assert(label, 'visible answer for selection');
    const target = await page.evaluate(label => {
      const where = window.fixture.locate(label);
      return { start: window.fixture.point(where), end: window.fixture.point(where, 'end') };
    }, label);
    await page.keyboard.down('Shift');
    await page.mouse.move(target.start.x, target.start.y);
    await page.mouse.down();
    await page.mouse.move(target.end.x, target.end.y, { steps: 12 });
    await page.mouse.up();
    await page.keyboard.up('Shift');
    assert.equal(await page.evaluate(() => window.fixture.terminal.getSelection()), label);
    fs.writeFileSync(path.join(config.root, 'prose-release'), 'release');
    await page.waitForFunction(() => window.fixture.viewport().includes('Σ'));
    assert.equal(await page.evaluate(() => window.fixture.terminal.getSelection()), label, 'new output preserves selection');
    report.checks.push('native Shift+drag copies exact visible text and survives output');

    await page.evaluate(() => window.fixture.resize(64, 36));
    await page.waitForFunction(() => window.fixture.terminal.cols === 64 && window.fixture.viewport().includes('draft界'));
    await page.screenshot({ path: path.join(config.logRoot, 'resized.png') });
    await page.locator('.xterm-helper-textarea').focus();
    await page.keyboard.press('Control+u');
    await page.waitForFunction(() => window.fixture.viewport().split('\n').some(row => row.trimEnd() === '❯'));
    for (let index = 0; index < sampling.input; index++) {
      const draft = 'x'.repeat(index + 1);
      await page.evaluate(draft => window.fixture.armInput(draft), draft);
      await page.keyboard.press('x');
      const input = await page.evaluate(() => window.fixture.waitForInput());
      report.samples.push(input.parsed - input.keydown);
    }
    report.parsed_key_p95_ms = [...report.samples].sort((a, b) => a - b)[Math.ceil(report.samples.length * .95) - 1];
    await page.screenshot({ path: path.join(config.logRoot, 'typed.png') });
    report.processes_after = (await cdp.send('SystemInfo.getProcessInfo')).processInfo;
    report.checks.push(`draft survives resize; ${sampling.input} real browser key events`);
    report.status = 'complete';
  } catch (error) {
    report.status = 'failed'; report.error = String(error.stack || error); process.exitCode = 1;
  } finally {
    if (page) {
      report.final_text = await page.evaluate(() => window.fixture?.viewport()).catch(() => '');
      await page.screenshot({ path: path.join(config.logRoot, 'final.png') }).catch(() => {});
    }
    fs.writeFileSync(config.report, JSON.stringify(report, null, 2));
    fs.writeFileSync(path.join(config.logRoot, 'wire.txt'), wire.join(''));
    if (child && !exited) child.kill();
    if (browser) await browser.close();
    for (const client of websocket.clients) client.terminate();
    websocket.close(); server.close();
  }
})().then(() => process.exit(process.exitCode || 0)).catch(error => { console.error(error); process.exit(1); });
