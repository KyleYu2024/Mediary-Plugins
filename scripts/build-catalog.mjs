import { readFile, readdir, writeFile } from 'node:fs/promises';
import path from 'node:path';

const root = path.resolve(import.meta.dirname, '..');
const pluginsDir = path.join(root, 'plugins');
const outputPath = path.join(root, 'catalog.json');
const idPattern = /^[a-z0-9][a-z0-9._-]{0,63}$/;
const versionPattern = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$/;
const hashPattern = /^[a-f0-9]{64}$/;
const platforms = new Set(['linux-amd64', 'linux-arm64', 'darwin-amd64', 'darwin-arm64']);
const allowedKeys = new Set([
  'id',
  'name',
  'version',
  'description',
  'author',
  'homepage',
  'source',
  'license',
  'api_version',
  'min_mediary_version',
  'permissions',
  'artifacts',
]);

function requireHttps(value, field) {
  let url;
  try {
    url = new URL(value);
  } catch {
    throw new Error(`${field} 必须是有效 URL`);
  }
  if (url.protocol !== 'https:') {
    throw new Error(`${field} 必须使用 HTTPS`);
  }
}

function validatePlugin(plugin, filename) {
  if (!plugin || typeof plugin !== 'object' || Array.isArray(plugin)) {
    throw new Error(`${filename}: 顶层必须是对象`);
  }
  for (const key of Object.keys(plugin)) {
    if (!allowedKeys.has(key)) throw new Error(`${filename}: 不支持字段 ${key}`);
  }
  if (!idPattern.test(plugin.id ?? '')) throw new Error(`${filename}: id 无效`);
  if (filename !== `${plugin.id}.json`) throw new Error(`${filename}: 文件名必须与 id 一致`);
  for (const field of ['name', 'description', 'author']) {
    if (typeof plugin[field] !== 'string' || !plugin[field].trim()) {
      throw new Error(`${filename}: ${field} 不能为空`);
    }
  }
  if (!versionPattern.test(plugin.version ?? '')) throw new Error(`${filename}: version 无效`);
  if (!versionPattern.test(plugin.min_mediary_version ?? '')) {
    throw new Error(`${filename}: min_mediary_version 无效`);
  }
  if (plugin.api_version !== 1) throw new Error(`${filename}: api_version 只支持 1`);
  requireHttps(plugin.homepage, `${filename}: homepage`);
  if (plugin.source !== undefined) requireHttps(plugin.source, `${filename}: source`);
  if (!Array.isArray(plugin.permissions) || !plugin.permissions.every((item) => typeof item === 'string')) {
    throw new Error(`${filename}: permissions 必须是字符串数组`);
  }
  if (!plugin.artifacts || typeof plugin.artifacts !== 'object' || Array.isArray(plugin.artifacts)) {
    throw new Error(`${filename}: artifacts 必须是对象`);
  }
  const entries = Object.entries(plugin.artifacts);
  if (!entries.length) throw new Error(`${filename}: 至少提供一个平台包`);
  for (const [platform, artifact] of entries) {
    if (!platforms.has(platform)) throw new Error(`${filename}: 不支持平台 ${platform}`);
    if (!artifact || typeof artifact !== 'object' || Array.isArray(artifact)) {
      throw new Error(`${filename}: ${platform} 包信息无效`);
    }
    requireHttps(artifact.url, `${filename}: ${platform}.url`);
    if (!hashPattern.test(artifact.sha256 ?? '')) {
      throw new Error(`${filename}: ${platform}.sha256 无效`);
    }
    if (artifact.size !== undefined && (!Number.isInteger(artifact.size) || artifact.size < 1 || artifact.size > 104857600)) {
      throw new Error(`${filename}: ${platform}.size 无效`);
    }
  }
}

const filenames = (await readdir(pluginsDir))
  .filter((filename) => filename.endsWith('.json'))
  .sort((left, right) => left.localeCompare(right));
const plugins = [];
const ids = new Set();

for (const filename of filenames) {
  const raw = await readFile(path.join(pluginsDir, filename), 'utf8');
  const plugin = JSON.parse(raw);
  validatePlugin(plugin, filename);
  if (ids.has(plugin.id)) throw new Error(`${filename}: id 重复`);
  ids.add(plugin.id);
  plugins.push(plugin);
}

const catalog = `${JSON.stringify({ schema_version: 1, plugins }, null, 2)}\n`;
await writeFile(outputPath, catalog);
console.log(`catalog.json 已生成，共 ${plugins.length} 个插件`);
