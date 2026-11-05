// Synthetic Pi-compatible collector qualification client, not the installed Pi.
const fs = require('fs');
const { spawnSync } = require('child_process');
if (process.argv.includes('--version')) {
  fs.writeSync(1, 'pi ASB-fixed-work-fixture-1\n');
  process.exit(0);
}
if (process.argv[2] === '--worker') {
  const iterations = Number(process.argv[3]);
  let value = 0;
  for (let i = 0; i < iterations; i++) value = (Math.imul(value ^ i, 1664525) + 1013904223) | 0;
  fs.writeSync(1, String(value) + '\n');
  process.exit(0);
}
async function main() {
  const iterations = Number(process.env.ASB_FIXED_WORK_ITERATIONS);
  if (!Number.isSafeInteger(iterations) || iterations < 1 || iterations > 1000000000) throw Error('invalid fixed work budget');
  const config = JSON.parse(fs.readFileSync(process.env.PI_CODING_AGENT_DIR + '/models.json'));
  const response = await fetch(config.providers.asb.baseUrl + '/responses', {
    method: 'POST', headers: {'content-type': 'application/json'},
    body: JSON.stringify({model:'test-model', input:'offline', tools:[{type:'function'}]})
  });
  if (!response.ok) throw Error('local fixture replay rejected request');
  await response.arrayBuffer();
  const checksums = [];
  for (let child = 0; child < 3; child++) {
    const result = spawnSync(process.execPath, [__filename, '--worker', String(iterations)], {encoding:'utf8'});
    if (result.status !== 0) throw Error('fixed-work child failed');
    checksums.push(result.stdout.trim());
  }
  if (new Set(checksums).size !== 1) throw Error('fixed work checksums differ');
  const buffer = Buffer.alloc(1024 * 1024, 1);
  for (let block = 0; block < 128; block++) {
    fs.writeFileSync('fixed-work.bin', buffer);
    if (!fs.readFileSync('fixed-work.bin').equals(buffer)) throw Error('short fixed I/O');
  }
  fs.unlinkSync('fixed-work.bin');
  fs.mkdirSync('src', {recursive:true});
  fs.writeFileSync('src/parser.py', "def parse_path(value: str) -> list[str]:\n    return [segment for segment in value.split('/') if segment]\n");
  fs.writeSync(1, 'ASB_FIXED_WORK=' + JSON.stringify({iterations, children:3, io_blocks:128, io_block_bytes:buffer.length, checksums}) + '\n');
}
main().then(() => process.exit(0)).catch(error => { console.error(error.message); process.exitCode = 1; });
