import { createHash } from 'node:crypto';
import { readFile, stat, writeFile } from 'node:fs/promises';
import path from 'node:path';

const [releaseTag, distArgument] = process.argv.slice(2);
if (!releaseTag || !distArgument) {
  throw new Error('用法: node scripts/update-official-catalog.mjs <release-tag> <dist-dir>');
}
if (!/^plugins-v[0-9A-Za-z.-]+$/.test(releaseTag)) {
  throw new Error(`发布 tag 无效: ${releaseTag}`);
}

const root = path.resolve(import.meta.dirname, '..');
const distDir = path.resolve(distArgument);
const platforms = ['linux-amd64', 'linux-arm64', 'darwin-amd64', 'darwin-arm64'];
const pluginIds = [
  'maoyan-rank',
  'pansou',
  'tmdb-trending',
  'subscribe-reminder',
  'bark-notify',
];
const minimumMediaryVersions = {
  'maoyan-rank': '0.7.7',
  pansou: '0.7.7',
  'tmdb-trending': '0.8.1',
  'subscribe-reminder': '0.8.10',
  'bark-notify': '1.7.4',
};
const releaseBase = `https://github.com/KyleYu2024/Mediary-Plugins/releases/download/${releaseTag}`;

for (const id of pluginIds) {
  const manifestPath = path.join(root, 'official', id, 'plugin.json');
  const manifest = JSON.parse(await readFile(manifestPath, 'utf8'));
  if (manifest.id !== id) {
    throw new Error(`${manifestPath}: manifest.id 与目录名不一致`);
  }

  const artifacts = {};
  for (const platform of platforms) {
    const filename = `${id}-v${manifest.version}-${platform}.tar.gz`;
    const packagePath = path.join(distDir, filename);
    const bytes = await readFile(packagePath);
    const metadata = await stat(packagePath);
    artifacts[platform] = {
      url: `${releaseBase}/${filename}`,
      sha256: createHash('sha256').update(bytes).digest('hex'),
      size: metadata.size,
    };
  }

  const entry = {
    id,
    name: manifest.name,
    version: manifest.version,
    description: manifest.description,
    author: 'Mediary',
    homepage: `https://github.com/KyleYu2024/Mediary-Plugins/tree/${releaseTag}/official/${id}`,
    source: `https://github.com/KyleYu2024/Mediary-Plugins/tree/${releaseTag}/official`,
    license: 'MIT',
    api_version: manifest.api_version ?? 1,
    min_mediary_version: minimumMediaryVersions[id],
    permissions: manifest.requested_scopes ?? [],
    artifacts,
  };
  await writeFile(
    path.join(root, 'plugins', `${id}.json`),
    `${JSON.stringify(entry, null, 2)}\n`,
  );
}

console.log(`已生成 ${pluginIds.length} 个官方插件索引条目: ${releaseTag}`);
