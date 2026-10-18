const assert = require('node:assert/strict');
const { execFile } = require('node:child_process');
const fs = require('node:fs');
const http = require('node:http');
const os = require('node:os');
const path = require('node:path');
const { promisify } = require('node:util');
const test = require('node:test');

function removeFixture(directory) {
  const resolved = fs.realpathSync(directory);
  assert.equal(path.dirname(resolved).toLowerCase(), fs.realpathSync(os.tmpdir()).toLowerCase());
  assert.ok(path.basename(resolved).startsWith('scode-provider-probe-'));
  assert.equal(fs.lstatSync(directory).isSymbolicLink(), false);
  fs.rmSync(resolved, { recursive: true });
}

test('the real probe reports catalog and route failures without exposing credentials', async () => {
  const credential = 'fixture-provider-secret-12345678';
  const azureCredential = `${credential}-18`;
  const calls = [];
  const server = http.createServer(async (request, response) => {
    calls.push(`${request.method} ${request.url}`);
    if (request.url === '/v1/models') {
      assert.equal(request.headers.authorization, `Bearer ${credential}`);
      response.setHeader('content-type', 'application/json');
      response.end(JSON.stringify({ data: [{ id: 'fixture-model' }, { id: 'fixture-azure' }] }));
      return;
    }
    let body = '';
    for await (const chunk of request) body += chunk;
    const model = JSON.parse(body).model;
    assert.ok(['fixture-model', 'fixture-azure'].includes(model));
    const selectedCredential = model === 'fixture-azure' ? azureCredential : credential;
    assert.equal(request.headers.authorization, `Bearer ${selectedCredential}`);
    if (request.url === '/v1/messages' || model === 'fixture-azure') {
      response.writeHead(400, { 'content-type': 'application/json' });
      response.end(JSON.stringify({ error: { type: 'invalid_request_error', code: 'model_not_found',
        message: `No route for credential ${selectedCredential}` } }));
    } else {
      assert.equal(request.url, '/v1/chat/completions');
      response.writeHead(200, { 'content-type': 'text/event-stream' });
      response.end('data: {"usage":{"prompt_tokens":12,"completion_tokens":1,"total_tokens":13}}\n\ndata: [DONE]\n\n');
    }
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'scode-provider-probe-'));
  try {
    const output = path.join(directory, 'evidence.json');
    const { stdout } = await promisify(execFile)(process.execPath,
      [path.join(__dirname, 'probe_live_usage.cjs'), output], {
        env: { ...process.env, PROXY_AUTH_TOKEN: credential,
          PROXY_BASE_URL: `http://127.0.0.1:${server.address().port}/v1`,
          SCODE_LIVE_MODEL: 'fixture-model', SCODE_NATIVE_MODEL: '', SCODE_AZURE_MODEL: '',
          NATIVE_PROXY_AUTH_TOKEN: '', AZURE_PROXY_AUTH_TOKEN: '' },
        timeout: 15000, windowsHide: true,
      });
    const serialized = fs.readFileSync(output, 'utf8');
    assert.ok(!serialized.includes(credential));
    assert.ok(!stdout.includes(credential));
    const report = JSON.parse(serialized);
    assert.deepEqual(report[0], { route: 'models', profile: 'default', status: 200,
      models: ['fixture-azure', 'fixture-model'] });
    assert.equal(report[1].route, 'chat/completions');
    assert.equal(report[1].terminalSeen, true);
    assert.equal(report[1].usage[0].counts.total_tokens, 13);
    assert.equal(report[2].route, 'messages');
    assert.equal(report[2].status, 400);
    assert.equal(report[2].providerError.code, 'model_not_found');
    assert.equal(report[2].providerError.message, 'No route for credential [REDACTED]');
    assert.deepEqual(calls, ['GET /v1/models', 'POST /v1/chat/completions', 'POST /v1/messages']);
    const azureOutput = path.join(directory, 'azure-evidence.json');
    await promisify(execFile)(process.execPath,
      [path.join(__dirname, 'probe_live_usage.cjs'), azureOutput], {
        env: { ...process.env, PROXY_AUTH_TOKEN: credential,
          PROXY_BASE_URL: `http://127.0.0.1:${server.address().port}/v1`,
          SCODE_LIVE_MODEL: 'fixture-model', SCODE_NATIVE_MODEL: '',
          SCODE_AZURE_MODEL: 'fixture-azure', NATIVE_PROXY_AUTH_TOKEN: '',
          AZURE_PROXY_AUTH_TOKEN: azureCredential },
        timeout: 15000, windowsHide: true,
      });
    const azureSerialized = fs.readFileSync(azureOutput, 'utf8');
    assert.ok(!azureSerialized.includes(credential));
    assert.ok(!azureSerialized.includes(azureCredential));
    const azureReport = JSON.parse(azureSerialized);
    assert.ok(azureReport[0].models.includes('fixture-azure'));
    const azureRoutes = azureReport.filter(item => item.model === 'fixture-azure');
    assert.equal(azureRoutes.length, 2);
    assert.ok(azureRoutes.every(item => item.profile === 'azure'
      && item.providerError.message === 'No route for credential [REDACTED]'));
    assert.ok(azureRoutes.every(item => item.status === 400 && item.providerError.code === 'model_not_found'),
      'catalog presence must never be reported as a working provider route');
  } finally {
    server.closeAllConnections();
    await new Promise(resolve => server.close(resolve));
    removeFixture(directory);
  }
});

test('the same model is probed independently through each configured auth profile', async () => {
  const defaultCredential = 'fixture-shared-provider-12345678';
  const nativeCredential = `${defaultCredential}-7`;
  const azureCredential = `${defaultCredential}-18`;
  const calls = [];
  const server = http.createServer(async (request, response) => {
    const credential = request.headers.authorization?.slice('Bearer '.length);
    calls.push({ url: request.url, credential });
    if (request.url === '/v1/models') {
      assert.equal(credential, defaultCredential);
      response.writeHead(200, { 'content-type': 'application/json' });
      response.end(JSON.stringify({ data: [{ id: 'shared-model' }] }));
      return;
    }
    let body = '';
    for await (const chunk of request) body += chunk;
    assert.equal(JSON.parse(body).model, 'shared-model');
    assert.ok([defaultCredential, nativeCredential, azureCredential].includes(credential));
    if (credential === defaultCredential) {
      response.writeHead(400, { 'content-type': 'application/json' });
      response.end(JSON.stringify({ error: { code: 'model_not_found', message: defaultCredential } }));
    } else {
      response.writeHead(200, { 'content-type': 'text/event-stream' });
      response.end('data: {"usage":{"prompt_tokens":3,"completion_tokens":1}}\n\ndata: [DONE]\n\n');
    }
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'scode-provider-probe-'));
  const env = { ...process.env, PROXY_AUTH_TOKEN: defaultCredential,
    NATIVE_PROXY_AUTH_TOKEN: nativeCredential, AZURE_PROXY_AUTH_TOKEN: azureCredential,
    PROXY_BASE_URL: `http://127.0.0.1:${server.address().port}/v1`,
    SCODE_LIVE_MODEL: 'shared-model', SCODE_NATIVE_MODEL: 'shared-model', SCODE_AZURE_MODEL: 'shared-model' };
  try {
    const output = path.join(directory, 'evidence.json');
    await promisify(execFile)(process.execPath,
      [path.join(__dirname, 'probe_live_usage.cjs'), output], { env, timeout: 15000, windowsHide: true });
    const serialized = fs.readFileSync(output, 'utf8');
    assert.ok([defaultCredential, nativeCredential, azureCredential].every(key => !serialized.includes(key)));
    const report = JSON.parse(serialized);
    assert.equal(report.length, 7);
    for (const profile of ['default', 'native', 'azure']) {
      const routes = report.filter(item => item.model === 'shared-model' && item.profile === profile);
      assert.equal(routes.length, 2);
      assert.ok(routes.every(item => item.status === (profile === 'default' ? 400 : 200)));
      if (profile !== 'default') assert.ok(routes.every(item => item.terminalSeen));
    }
    const before = calls.length;
    await assert.rejects(promisify(execFile)(process.execPath,
      [path.join(__dirname, 'probe_live_usage.cjs'), path.join(directory, 'missing.json')], {
        env: { ...env, AZURE_PROXY_AUTH_TOKEN: '' }, timeout: 15000, windowsHide: true,
      }), error => error.stderr.includes('AZURE_PROXY_AUTH_TOKEN is required')
        && !error.stderr.includes(azureCredential));
    assert.equal(calls.length, before, 'missing Azure auth must fail before making any provider request');
  } finally {
    server.closeAllConnections();
    await new Promise(resolve => server.close(resolve));
    removeFixture(directory);
  }
});

test('the workflow gives the Azure CLI profile the same scoped credential as its probe', async () => {
  const workflow = fs.readFileSync(path.join(__dirname, '../workflows/rust-ci.yml'), 'utf8');
  const blocks = [...workflow.matchAll(/node - <<'NODE'\r?\n([\s\S]*?)\r?\n[ \t]+NODE/g)];
  const generators = blocks.filter(match => match[1].includes("'sudorouter-azure'"));
  assert.equal(generators.length, 1);
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'scode-provider-probe-'));
  const env = { ...process.env, SUDO_CODE_CONFIG_HOME: directory,
    PROXY_AUTH_TOKEN: 'fixture-default-provider-12345678',
    AZURE_PROXY_AUTH_TOKEN: 'fixture-scoped-azure-provider-87654321', SCODE_AZURE_CHANNEL: '18' };
  try {
    await promisify(execFile)(process.execPath, ['-e', generators[0][1]],
      { env, timeout: 15000, windowsHide: true });
    const config = JSON.parse(fs.readFileSync(path.join(directory, 'sudocode.json'), 'utf8'));
    assert.equal(config.auth_modes.proxy.sudorouter.apiKey, env.PROXY_AUTH_TOKEN);
    assert.equal(config.auth_modes.proxy['sudorouter-azure'].apiKey, env.AZURE_PROXY_AUTH_TOKEN);
    assert.equal(config.auth_modes.proxy['sudorouter-azure'].baseUrl, config.auth_modes.proxy.sudorouter.baseUrl);
    await assert.rejects(promisify(execFile)(process.execPath, ['-e', generators[0][1]], {
      env: { ...env, AZURE_PROXY_AUTH_TOKEN: '' }, timeout: 15000, windowsHide: true,
    }), error => error.stderr.includes('AZURE_PROXY_AUTH_TOKEN is required')
      && !error.stderr.includes(env.AZURE_PROXY_AUTH_TOKEN));
  } finally {
    removeFixture(directory);
  }
});
