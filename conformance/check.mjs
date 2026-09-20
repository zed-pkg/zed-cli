#!/usr/bin/env node
import { readFile } from 'node:fs/promises';
import { spawnSync } from 'node:child_process';

const cargoPath = 'Cargo.toml';
const cargo = await readFile(cargoPath, 'utf8');

const bindings = [
  {
    crate: 'zed-interfaces',
    file: 'contracts/zed-interfaces.upstream-authority.v1.json',
    repository: 'zed-pkg/zed-interfaces',
  },
  {
    crate: 'zed-lib',
    file: 'contracts/zed-lib.upstream-authority.v1.json',
    repository: 'zed-pkg/zed-lib-core',
  },
];

function escapeRegex(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

function cargoRevision(crateName) {
  const pattern = new RegExp(
    `^${escapeRegex(crateName)}\\s*=\\s*\\{[^\\n]*\\brev\\s*=\\s*"([0-9a-f]{40})"[^\\n]*\\}\\s*$`,
    'm',
  );
  const match = cargo.match(pattern);
  if (!match) {
    throw new Error(`${cargoPath}: missing exact 40-hex rev for ${crateName}`);
  }
  return match[1];
}

for (const binding of bindings) {
  const document = JSON.parse(await readFile(binding.file, 'utf8'));
  if (document?.schema !== 'ores.contract-authority-binding/v1') {
    throw new Error(`${binding.file}: invalid authority-binding schema`);
  }
  if (document.repository !== binding.repository) {
    throw new Error(`${binding.file}: repository drift`);
  }
  if (document.path !== 'contracts/' || document.mode !== 'external-pinned') {
    throw new Error(`${binding.file}: authority path/mode drift`);
  }
  const consumed = cargoRevision(binding.crate);
  if (document.commit !== consumed) {
    throw new Error(
      `${binding.file}: pinned authority ${document.commit} does not equal ${binding.crate} rev ${consumed} consumed by ${cargoPath}`,
    );
  }
}

const result = spawnSync(process.execPath, ['conformance/boundary.mjs'], {
  stdio: 'inherit',
  env: process.env,
});
if (result.error) throw result.error;
if (result.status !== 0) process.exit(result.status ?? 1);

console.log('zed-cli authority bindings match consumed immutable revisions');
