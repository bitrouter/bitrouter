// Offline fixture packaging; the provider/process capture itself is Cargo-built Rust.
// SQLite backup reads committed WAL pages: https://www.sqlite.org/backup.html
import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { zstdCompressSync, zstdDecompressSync } from 'node:zlib';

const [capture, destination] = process.argv.slice(2);
if (!capture || !destination) throw new Error('usage: node export.mjs CAPTURE NEW_DESTINATION');
fs.mkdirSync(destination);
const sha256 = bytes => createHash('sha256').update(bytes).digest('hex');
const scratch = fs.mkdtempSync(path.join(os.tmpdir(), 'bitrouter-baseline-export-'));
try {
    const snapshot = path.join(scratch, 'snapshot.sqlite');
    const database = path.resolve(capture, 'bitrouter.db');
    execFileSync('sqlite3', ['-readonly', database, `.backup ${JSON.stringify(snapshot)}`]);
    const uri = `file:${snapshot}?immutable=1`;
    const sqlite = query => execFileSync('sqlite3', ['-json', '-readonly', uri, query], {
        maxBuffer: 16 * 1024 * 1024,
        encoding: 'utf8',
    });
    const integrity = JSON.parse(sqlite('PRAGMA integrity_check'));
    if (integrity.length !== 1 || integrity[0].integrity_check !== 'ok') {
        throw new Error('snapshot integrity check failed');
    }
    const rows = JSON.parse(sqlite(
        'SELECT execution_id,sequence,payload FROM bro_execution_records ORDER BY execution_id,sequence',
    ));
    const json = rows.map(({ payload }) => {
        if (!payload.startsWith('bro-zstd-v2:')) return payload;
        const envelope = JSON.parse(payload.slice('bro-zstd-v2:'.length));
        if (!(envelope.bytes > 0 && envelope.bytes <= 4 * 1024 * 1024)) {
            throw new Error('invalid decoded record bound');
        }
        let bytes = zstdDecompressSync(Buffer.from(envelope.body, 'base64'), {
            maxOutputLength: envelope.bytes,
        });
        if (envelope.base64_field) {
            const { offset, bytes: size } = envelope.base64_field;
            if (offset < 0 || size < 0 || offset + size > bytes.length) {
                throw new Error('invalid packed Base64 range');
            }
            bytes = Buffer.concat([
                bytes.subarray(0, offset),
                Buffer.from(bytes.subarray(offset, offset + size).toString('base64')),
                bytes.subarray(offset + size),
            ]);
        }
        if (bytes.length !== envelope.bytes || sha256(bytes) !== envelope.sha256) {
            throw new Error('record size or digest mismatch');
        }
        return bytes.toString('utf8');
    });
    // Preserve each writer's JSON and all nested checkpoint strings verbatim.
    const journal = Buffer.from(`[${json.join(',')}]`);
    JSON.parse(journal.toString('utf8'));
    const bytes = fs.readFileSync(snapshot);
    const compressedDatabase = zstdCompressSync(bytes);
    const compressedJournal = zstdCompressSync(journal);
    fs.writeFileSync(path.join(destination, 'database.sqlite.zst'), compressedDatabase);
    fs.writeFileSync(path.join(destination, 'journal.json.zst'), compressedJournal);
    fs.writeFileSync(path.join(destination, 'export.json'), JSON.stringify({
        source_main_sha256: sha256(fs.readFileSync(database)),
        source_wal_sha256: fs.existsSync(`${database}-wal`)
            ? sha256(fs.readFileSync(`${database}-wal`)) : null,
        database_sha256: sha256(bytes),
        compressed_database_sha256: sha256(compressedDatabase),
        journal_sha256: sha256(journal),
        compressed_journal_sha256: sha256(compressedJournal),
        execution_roots: JSON.parse(sqlite('SELECT id,format_version,version FROM bro_executions')),
        records: rows.length,
        migration_rows: JSON.parse(sqlite('SELECT count(*) AS count FROM seaql_migrations'))[0].count,
    }, null, 2) + '\n');
} finally {
    fs.rmSync(scratch, { recursive: true, force: true });
}
