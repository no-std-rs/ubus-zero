import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { stripTypeScriptTypes } from 'node:module';
import test from 'node:test';
import vm from 'node:vm';

// Execute the actual program's routing with mocked host operations. Runnerless's
// proposed-program check independently compiles the AssemblyScript SDK calls.
const source = stripTypeScriptTypes(readFileSync(new URL('../.runnerless-ci.ts', import.meta.url), 'utf8'))
  .replace(/^import\s*\{[\s\S]*?\}\s*from\s*"runnerless\/v1";\s*/, '')
  .replace(/^export function /gm, 'function ');

class Json {
  constructor(value) { this.value = value; }
  get(key) { return new Json(this.value?.[key] ?? null); }
  string() { return typeof this.value === 'string' ? this.value : ''; }
  isNull() { return this.value === null; }
  get json() { return JSON.stringify(this.value); }
}

function execute(details, version = '0.4.1', releasePublished = true) {
  const calls = [];
  const declarations = [];
  class OperationRequest {
    constructor(key) { this.key = key; this.dependencies = []; }
    after(reference) { this.dependencies.push(reference); return this; }
  }
  class RegistryPublishRequest {
    constructor(tag, version) { this.tag = tag; this.version = version; this.packages = []; }
    add(name, asset) { this.packages.push([name, asset]); return this; }
  }
  const stage = (operation, request) => {
    const reference = `op:${request.key}`;
    calls.push({ operation, key: request.key, dependencies: [...request.dependencies] });
    return reference;
  };
  vm.runInNewContext(source + '\nconfigure(); run();', {
    workflow: () => ({ releasePlease() { return this; }, configure() {}, run() {} }),
    configuration: { workflow: (id, events) => declarations.push({ id, events: [...events] }) },
    event: { details: () => new Json(details) },
    releases: {
      finalize: request => stage('finalize', request),
      propose: request => stage('propose', request),
    },
    OperationRequest,
    RegistryPublishRequest,
    OperationRef: class { constructor(ref) { this.ref = ref; } },
    jobs: { lookup: () => ({ ok: false }) },
    checks: { report: (name, passed, summary) => calls.push({ operation: 'check', name, passed, summary }) },
    reviews: { evaluate: () => ({ state: 'success', summary: 'clean' }) },
    repo: { readText: (path, ref) => {
      calls.push({ operation: 'read', path, ref });
      return path === 'version.txt' ? `${version}\n` : 'name = "ubusd"';
    } },
    packages: {
      releasePublished: () => releasePublished,
      crates: (operation, request) => calls.push({ operation: 'crates', key: operation.key,
        tag: request.tag, version: request.version, packages: request.packages }),
    },
  });
  return { calls, declarations };
}

const mainBuild = {
  event: 'workflow_run', action: 'completed',
  workflowPath: '.github/workflows/ci.yml', conclusion: 'success',
  pullNumber: null, headRef: 'main', baseRef: 'main',
};

test('a successful main CI completion retries the shared recipe operations in order', () => {
  const { calls, declarations } = execute(mainBuild);
  assert.deepEqual(calls, [
    { operation: 'finalize', key: 'release-finalize', dependencies: [] },
    { operation: 'propose', key: 'release-propose', dependencies: ['op:release-finalize'] },
  ]);
  assert(declarations.some(({ events }) => events.includes('workflow_run')));
  assert(declarations.some(({ events }) => events.includes('runnerless_completion')));
});

test('PR, tag, incomplete, failed and unrelated workflow runs do not recover releases', () => {
  for (const change of [
    { action: 'requested' }, { action: 'in_progress' },
    { conclusion: 'failure' }, { conclusion: 'cancelled' }, { conclusion: 'skipped' },
    { conclusion: null }, { pullNumber: 41 },
    { headRef: 'feature' }, { headRef: 'ubus-zero-v0.4.1' },
    { workflowPath: '.github/workflows/firmware.yml' },
    { workflowPath: '.github/workflows/unknown.yml' },
    { workflowPath: null }, { headRef: null }, { baseRef: null },
    { headRef: null, baseRef: null }, { headRef: '', baseRef: '' },
  ]) {
    assert.deepEqual(execute({ ...mainBuild, ...change }).calls, [], JSON.stringify(change));
  }
});

test('tagged asset completion still publishes all three packages from its pinned revision', () => {
  const { calls } = execute({ ...mainBuild, workflowPath: '.github/workflows/publish-release.yml',
    headRef: 'ubus-zero-v0.4.1' });
  assert.deepEqual(calls, [
    { operation: 'read', path: 'version.txt', ref: 'head' },
    { operation: 'read', path: 'crates/ubusd/Cargo.toml', ref: 'head' },
    { operation: 'crates', key: 'publish-ubus-zero', tag: 'ubus-zero-v0.4.1', version: '0.4.1',
      packages: [
        ['ubus-zero', 'ubus-zero-0.4.1.cargo-upload'],
        ['ubusd', 'ubusd-0.4.1.cargo-upload'],
        ['ubus-client', 'ubus-client-0.4.1.cargo-upload'],
      ] },
    { operation: 'check', name: 'Crate publishing trigger', passed: true,
      summary: 'Publication requested for ubus-zero-v0.4.1. See crates-ubus-zero for the registry outcome.' },
  ]);
});

test('asset completion with a mismatched tag never requests publication', () => {
  const { calls } = execute({ ...mainBuild, workflowPath: '.github/workflows/publish-release.yml',
    headRef: 'ubus-zero-v0.4.0' });
  assert.deepEqual(calls, [
    { operation: 'read', path: 'version.txt', ref: 'head' },
    { operation: 'check', name: 'Crate publishing trigger', passed: false,
      summary: 'The workflow tag does not match version.txt at its pinned revision.' },
  ]);
});

test('asset failures and invalid refs explain why publication was not requested', () => {
  const assets = { ...mainBuild, workflowPath: '.github/workflows/publish-release.yml',
    headRef: 'ubus-zero-v0.4.1' };
  for (const conclusion of ['failure', 'cancelled', 'skipped', null]) {
    assert.deepEqual(execute({ ...assets, conclusion }).calls, [
      { operation: 'check', name: 'Crate publishing trigger', passed: false,
        summary: 'The release-asset workflow did not succeed.' },
    ]);
  }
  for (const headRef of ['main', 'feature', '', null]) {
    assert.deepEqual(execute({ ...assets, headRef }).calls, [
      { operation: 'check', name: 'Crate publishing trigger', passed: false,
        summary: 'The release-asset workflow did not run on a ubus-zero release tag.' },
    ]);
  }
  const missingRelease = execute(assets, '0.4.1', false).calls;
  assert.deepEqual(missingRelease, [
    { operation: 'read', path: 'version.txt', ref: 'head' },
    { operation: 'check', name: 'Crate publishing trigger', passed: false,
      summary: 'No stable published release matches the workflow tag and commit.' },
  ]);
  for (const action of ['requested', 'in_progress']) {
    assert.deepEqual(execute({ ...assets, action }).calls, []);
  }
});

test('review and completion events never stage release recovery', () => {
  assert.deepEqual(execute({ event: 'runnerless_completion' }).calls, []);
  assert.deepEqual(execute({ event: 'issue_comment', pullNumber: 41, state: 'open' }).calls,
    [{ operation: 'check', name: 'Codex review', passed: true, summary: 'clean' }]);
});
