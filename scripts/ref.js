// Reference crypto values computed with the SAME logic as og/cli crypto.service.ts
// Used to cross-check the Rust port. Run: node scripts/ref.js
const { createCipheriv, createDecipheriv, createHash, pbkdf2Sync } = require('node:crypto');
const { mnemonicToSeedSync } = require('../og/node_modules/bip39');

const SECRET = '6KYQBP847D4ATSFA';

function getKeyAndIvFrom(secret, salt) {
  const TRANSFORM_ROUNDS = 3;
  const password = Buffer.concat([Buffer.from(secret, 'utf8'), salt]);
  const md5Hashes = [];
  let digest = password;
  for (let i = 0; i < TRANSFORM_ROUNDS; i++) {
    md5Hashes[i] = createHash('md5').update(digest).digest();
    digest = Buffer.concat([md5Hashes[i], password]);
  }
  const key = Buffer.concat([md5Hashes[0], md5Hashes[1]]);
  const iv = md5Hashes[2];
  return { key, iv };
}

function encryptTextWithKey(text, secret, saltHex) {
  const salt = Buffer.from(saltHex, 'hex'); // fixed salt for determinism
  const { key, iv } = getKeyAndIvFrom(secret, salt);
  const cipher = createCipheriv('aes-256-cbc', key, iv);
  const encrypted = Buffer.concat([cipher.update(text, 'utf8'), cipher.final()]);
  return Buffer.concat([Buffer.from('Salted__'), salt, encrypted]).toString('hex');
}

function decryptTextWithKey(encryptedHex, secret) {
  const c = Buffer.from(encryptedHex, 'hex');
  const salt = c.subarray(8, 16);
  const { key, iv } = getKeyAndIvFrom(secret, salt);
  const decipher = createDecipheriv('aes-256-cbc', key, iv);
  return Buffer.concat([decipher.update(c.subarray(16)), decipher.final()]).toString('utf8');
}

function passToHash(password, saltHex) {
  return pbkdf2Sync(password, Buffer.from(saltHex, 'hex'), 10000, 32, 'sha1').toString('hex');
}

function GetFileDeterministicKey(key, data) {
  return createHash('sha512').update(key).update(data).digest();
}
function generateFileKey(mnemonic, bucketId, indexHex) {
  const seed = mnemonicToSeedSync(mnemonic);
  const bucketKey = GetFileDeterministicKey(seed, Buffer.from(bucketId, 'hex'));
  return GetFileDeterministicKey(bucketKey.subarray(0, 32), Buffer.from(indexHex, 'hex')).subarray(0, 32).toString('hex');
}

const mnemonic = 'legal winner thank year wave sausage worth useful legal winner thank yellow';
const bucketId = '0123456789abcdef0123456789abcdef';
const indexHex = 'a'.repeat(64);
const saltHex = '00112233445566778899aabbccddeeff'.slice(0, 16); // 8 bytes
const encHello = encryptTextWithKey('hello world', SECRET, saltHex);

console.log(JSON.stringify({
  enc_hello: encHello,
  dec_hello: decryptTextWithKey(encHello, SECRET),
  pass_hash: passToHash('mypassword', 'deadbeef'),
  file_key: generateFileKey(mnemonic, bucketId, indexHex),
  ctr_sample: (() => {
    const key = Buffer.from(generateFileKey(mnemonic, bucketId, indexHex), 'hex');
    const iv = Buffer.from(indexHex, 'hex').subarray(0, 16);
    const cipher = createCipheriv('aes-256-ctr', key, iv);
    return Buffer.concat([cipher.update(Buffer.from('the quick brown fox')), cipher.final()]).toString('hex');
  })(),
  shard_hash: (() => {
    const data = Buffer.from('encrypted-shard-content');
    return createHash('ripemd160').update(createHash('sha256').update(data).digest()).digest('hex');
  })(),
}, null, 2));
