#!/usr/bin/env node

import fs from 'node:fs';
import path from 'node:path';
import process from 'node:process';

const IGNORED_DIRS = new Set([
  '.git',
  '.zed',
  '.zpkg',
  'node_modules',
  'target',
  'dist',
  'build',
  'coverage',
]);

function fail(message) {
  console.error(`node-target-discovery: ${message}`);
  process.exitCode = 2;
  return [];
}

function readJson(file) {
  try {
    return JSON.parse(fs.readFileSync(file, 'utf8'));
  } catch (error) {
    throw new Error(`cannot parse ${file}: ${error.message}`);
  }
}

function normalizeRelative(root, dir) {
  const relative = path.relative(root, dir) || '.';
  return relative.split(path.sep).join('/');
}

function walkPackages(root, current = root, output = []) {
  for (const entry of fs.readdirSync(current, { withFileTypes: true })) {
    if (entry.isDirectory()) {
      if (!IGNORED_DIRS.has(entry.name)) {
        walkPackages(root, path.join(current, entry.name), output);
      }
      continue;
    }
    if (entry.isFile() && entry.name === 'package.json') {
      const file = path.join(current, entry.name);
      output.push({
        dir: normalizeRelative(root, current),
        file,
        manifest: readJson(file),
      });
    }
  }
  return output;
}

function workspacePatterns(manifest) {
  const value = manifest?.workspaces;
  if (Array.isArray(value)) return value.filter((item) => typeof item === 'string');
  if (value && Array.isArray(value.packages)) {
    return value.packages.filter((item) => typeof item === 'string');
  }
  return [];
}

function patternRegex(pattern) {
  const normalized = pattern.replace(/^\.\//, '').replace(/\/$/, '');
  const escaped = normalized
    .replace(/[.+^${}()|[\]\\]/g, '\\$&')
    .replace(/\*\*/g, '\u0000')
    .replace(/\*/g, '[^/]+')
    .replace(/\u0000/g, '.+');
  return new RegExp(`^${escaped}$`);
}

function explicitTargets(rootManifest) {
  const targets = rootManifest?.zed?.canonicalNodeTargets;
  if (targets === undefined) return null;
  if (!Array.isArray(targets) || targets.some((item) => typeof item !== 'string' || !item.trim())) {
    throw new Error('package.json zed.canonicalNodeTargets must be an array of non-empty strings');
  }
  return [...new Set(targets.map((item) => item.replace(/^\.\//, '').replace(/\/$/, '') || '.'))];
}

function runnableScripts(manifest) {
  const scripts = manifest?.scripts ?? {};
  return ['typecheck', 'build', 'test'].filter((name) => typeof scripts[name] === 'string');
}

export function discoverNodeTargets(rootInput) {
  const root = fs.realpathSync(rootInput);
  const packages = walkPackages(root).sort((left, right) => left.dir.localeCompare(right.dir));
  const rootPackage = packages.find((item) => item.dir === '.');
  if (!rootPackage) {
    throw new Error(`no root package.json found under ${root}`);
  }

  const byDir = new Map(packages.map((item) => [item.dir, item]));
  const explicit = explicitTargets(rootPackage.manifest);
  if (explicit !== null) {
    for (const target of explicit) {
      if (!byDir.has(target)) {
        throw new Error(`explicit canonical Node target ${target} has no package.json`);
      }
    }
    return explicit.map((dir) => ({ ...byDir.get(dir), reason: 'explicit' }));
  }

  const patterns = workspacePatterns(rootPackage.manifest);
  if (patterns.length > 0) {
    const regexes = patterns.map(patternRegex);
    const selected = packages.filter(
      (item) => item.dir !== '.' && regexes.some((regex) => regex.test(item.dir)),
    );
    if (selected.length === 0) {
      throw new Error(`workspace declarations matched no package.json targets: ${patterns.join(', ')}`);
    }
    return selected.map((item) => ({ ...item, reason: 'workspace' }));
  }

  const nestedPublishable = packages.filter(
    (item) => item.dir !== '.' && item.manifest?.private !== true,
  );
  if (nestedPublishable.length === 1) {
    return [{ ...nestedPublishable[0], reason: 'single-nested-publishable' }];
  }
  if (nestedPublishable.length > 1) {
    throw new Error(
      `ambiguous canonical Node targets (${nestedPublishable.map((item) => item.dir).join(', ')}); ` +
        'declare package.json zed.canonicalNodeTargets or workspaces',
    );
  }

  if (packages.length === 1) {
    return [{ ...rootPackage, reason: 'root-only' }];
  }

  const nestedRunnable = packages.filter(
    (item) => item.dir !== '.' && runnableScripts(item.manifest).length > 0,
  );
  if (rootPackage.manifest?.private === true && nestedRunnable.length === 1) {
    return [{ ...nestedRunnable[0], reason: 'private-tooling-root-single-runnable-nested' }];
  }

  throw new Error(
    'no unambiguous canonical Node target; declare package.json zed.canonicalNodeTargets or workspaces',
  );
}

function main() {
  const root = process.argv[2] ?? '.';
  let targets;
  try {
    targets = discoverNodeTargets(root);
  } catch (error) {
    fail(error.message);
    return;
  }

  for (const target of targets) {
    const scripts = runnableScripts(target.manifest);
    process.stderr.write(
      `node-target-discovery: ${target.dir} (${target.reason}; scripts=${scripts.join(',') || 'none'})\n`,
    );
    process.stdout.write(`${target.dir}\n`);
  }
}

if (process.argv[1] && path.resolve(process.argv[1]) === path.resolve(new URL(import.meta.url).pathname)) {
  main();
}
