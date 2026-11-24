import {build} from 'esbuild';
import {readFile, writeFile} from 'node:fs/promises';
import {createHash} from 'node:crypto';
const result = await build({entryPoints:['install.js'],bundle:true,platform:'browser',format:'iife',
  external:['util'],outfile:'dist/timeline.js',metafile:true});
const packageRoots = new Set();
const sources = {};
for (const name of Object.keys(result.metafile.inputs)) {
  if (name.startsWith('(disabled):')) {sources[name]={disabled:true};continue;}
  sources[name] = createHash('sha256').update(await readFile(name)).digest('hex');
  const match = name.match(/^(.*node_modules\/(?:@[^/]+\/)?[^/]+)\//);
  if (match) packageRoots.add(match[1]);
}
const notices = [];
for (const root of [...packageRoots].sort()) {
  const metadata = JSON.parse(await readFile(root+'/package.json','utf8'));
  let license;
  for (const file of ['LICENSE','LICENSE.md','LICENSE.txt']) {
    try {license=await readFile(root+'/'+file,'utf8');break;} catch (error) {
      if (error.code !== 'ENOENT') throw error;
    }
  }
  if (!license) throw Error('Missing license for '+metadata.name);
  notices.push(`${metadata.name} ${metadata.version}\n${license}`);
}
await writeFile('dist/THIRD_PARTY_LICENSES.txt',notices.join('\n\n'));
await writeFile('dist/build.json',JSON.stringify({sources,
  bundleSha256:createHash('sha256').update(await readFile('dist/timeline.js')).digest('hex'),
},null,2)+'\n');
