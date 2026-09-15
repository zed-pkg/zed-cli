import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';

import { discoverNodeTargets } from './discover-node-targets.mjs';

function fixture(files) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'zed-node-targets-'));
  for (const [relative, json] of Object.entries(files)) {
    const file = path.join(root, relative);
    fs.mkdirSync(path.dirname(file), { recursive: true });
    fs.writeFileSync(file, `${JSON.stringify(json, null, 2)}\n`);
  }
  return root;
}

function dirs(root) {
  return discoverNodeTargets(root).map((target) => target.dir);
}

test('keeps a root-only package as the canonical target', () => {
  const root = fixture({
    'package.json': { name: 'root-runtime', scripts: { test: 'node --test' } },
  });
  assert.deepEqual(dirs(root), ['.']);
});

test('skips private tooling root when exactly one publishable nested package exists', () => {
  const root = fixture({
    'package.json': { name: 'codegen', private: true, scripts: { test: 'node --test' } },
    'src/ts/package.json': {
      name: '@example/runtime',
      scripts: { typecheck: 'tsc --noEmit', test: 'node --test' },
    },
  });
  assert.deepEqual(dirs(root), ['src/ts']);
});

test('compiles every declared workspace package exactly once', () => {
  const root = fixture({
    'package.json': { name: 'workspace-root', private: true, workspaces: ['packages/*'] },
    'packages/a/package.json': { name: '@example/a', scripts: { build: 'tsc' } },
    'packages/b/package.json': { name: '@example/b', private: true, scripts: { test: 'node --test' } },
    'tools/ignored/package.json': { name: '@example/ignored' },
  });
  assert.deepEqual(dirs(root), ['packages/a', 'packages/b']);
});

test('fails closed for multiple undeclared publishable nested packages', () => {
  const root = fixture({
    'package.json': { name: 'tooling', private: true },
    'clients/a/package.json': { name: '@example/a' },
    'clients/b/package.json': { name: '@example/b' },
  });
  assert.throws(() => discoverNodeTargets(root), /ambiguous canonical Node targets/);
});

test('explicit canonical targets resolve ambiguity deterministically', () => {
  const root = fixture({
    'package.json': {
      name: 'tooling',
      private: true,
      zed: { canonicalNodeTargets: ['clients/b', 'clients/b', './clients/a/'] },
    },
    'clients/a/package.json': { name: '@example/a' },
    'clients/b/package.json': { name: '@example/b' },
  });
  assert.deepEqual(dirs(root), ['clients/b', 'clients/a']);
});

test('private tooling root can select one runnable nested private package', () => {
  const root = fixture({
    'package.json': { name: 'tooling', private: true },
    'generated/node/package.json': {
      name: '@example/generated',
      private: true,
      scripts: { typecheck: 'tsc --noEmit' },
    },
  });
  assert.deepEqual(dirs(root), ['generated/node']);
});
