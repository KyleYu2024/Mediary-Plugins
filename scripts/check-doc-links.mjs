#!/usr/bin/env node

import { existsSync, readdirSync, readFileSync, statSync } from 'node:fs';
import { dirname, relative, resolve } from 'node:path';

const root = process.cwd();
const ignoredDirectories = new Set(['.git', 'node_modules', 'target']);

function markdownFiles(directory) {
  return readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    if (ignoredDirectories.has(entry.name)) return [];
    const path = resolve(directory, entry.name);
    if (entry.isDirectory()) return markdownFiles(path);
    return entry.isFile() && entry.name.endsWith('.md') ? [path] : [];
  });
}

const failures = [];
const linkPattern = /!?\[[^\]]*]\(([^)]+)\)/g;

for (const file of markdownFiles(root)) {
  const content = readFileSync(file, 'utf8');
  for (const match of content.matchAll(linkPattern)) {
    const rawTarget = match[1].trim().split(/\s+["']/)[0];
    if (
      !rawTarget
      || rawTarget.startsWith('#')
      || /^(?:https?:|mailto:)/.test(rawTarget)
    ) {
      continue;
    }

    const target = decodeURIComponent(rawTarget.split('#')[0]);
    const resolved = resolve(dirname(file), target);
    if (!resolved.startsWith(`${root}/`) || !existsSync(resolved)) {
      failures.push(`${relative(root, file)} -> ${rawTarget}`);
      continue;
    }

    if (target.endsWith('/') && !statSync(resolved).isDirectory()) {
      failures.push(`${relative(root, file)} -> ${rawTarget} (not a directory)`);
    }
  }
}

if (failures.length) {
  console.error('Invalid local documentation links:');
  failures.forEach((failure) => console.error(`- ${failure}`));
  process.exit(1);
}

console.log('Documentation links are valid.');
