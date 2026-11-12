<?php

declare(strict_types=1);

namespace EggDB;

final class KeyValue
{
    private const DECLARATIONS = <<<'C'
typedef struct eggdb_db eggdb_db;
typedef struct eggdb_txn eggdb_txn;
typedef struct eggdb_scan eggdb_scan;
typedef struct eggdb_options { uint32_t flags; uint32_t durability; uint64_t cache_bytes; } eggdb_options;
typedef struct eggdb_entry { const uint8_t *key; size_t key_len; const uint8_t *value; size_t value_len; } eggdb_entry;
typedef struct eggdb_collection_stats { uint64_t entries; uint64_t key_bytes; uint64_t value_bytes; } eggdb_collection_stats;
const char *eggdb_last_error(void);
const char *eggdb_last_sqlstate(void);
int eggdb_open(const char *path, const eggdb_options *options, eggdb_db **db);
int eggdb_close(eggdb_db *db);
void eggdb_free(uint8_t *data, size_t len);
int eggdb_begin_read(eggdb_db *db, eggdb_txn **txn);
int eggdb_begin_write(eggdb_db *db, eggdb_txn **txn);
int eggdb_commit(eggdb_txn *txn, uint64_t *stamp);
int eggdb_rollback(eggdb_txn *txn);
int eggdb_get(eggdb_txn *txn, const char *collection, const char *key, size_t key_len, uint8_t **value, size_t *value_len);
int eggdb_put(eggdb_txn *txn, const char *collection, const char *key, size_t key_len, const char *value, size_t value_len);
int eggdb_put_with_expiry(eggdb_txn *txn, const char *collection, const char *key, size_t key_len, const char *value, size_t value_len, uint64_t ttl_ms);
int eggdb_delete(eggdb_txn *txn, const char *collection, const char *key, size_t key_len, int *deleted);
int eggdb_scan_open(eggdb_txn *txn, const char *collection, const char *start, size_t start_len, const char *end, size_t end_len, uint32_t flags, eggdb_scan **scan);
int eggdb_scan_next(eggdb_txn *txn, eggdb_scan *scan, size_t max_entries, const eggdb_entry **entries, size_t *count);
void eggdb_scan_free(eggdb_scan *scan);
int eggdb_create_collection(eggdb_txn *txn, const char *name, int *created);
int eggdb_drop_collection(eggdb_txn *txn, const char *name, int *dropped);
int eggdb_rename_collection(eggdb_txn *txn, const char *name, const char *new_name);
int eggdb_collection_names(eggdb_txn *txn, uint8_t **names, size_t *names_len, size_t *count);
int eggdb_collection_stats(eggdb_txn *txn, const char *name, eggdb_collection_stats *stats);
C;

    private const OPEN_READ_ONLY = 1;

    private \FFI $ffi;
    private $database;

    public function __construct(string $path, public readonly bool $readOnly = false, ?string $library = null)
    {
        $library ??= getenv('EGGDB_LIBRARY') ?: (PHP_OS_FAMILY === 'Darwin' ? 'libeggdb_capi.dylib' : 'libeggdb_capi.so');
        $this->ffi = \FFI::cdef(self::DECLARATIONS, $library);
        $options = $this->ffi->new('eggdb_options');
        $options->flags = $readOnly ? self::OPEN_READ_ONLY : 0;
        $database = $this->ffi->new('eggdb_db*');
        self::check($this->ffi, $this->ffi->eggdb_open($path, \FFI::addr($options), \FFI::addr($database)));
        $this->database = $database;
    }

    public static function check(\FFI $ffi, int $status): void
    {
        if ($status === 0) {
            return;
        }
        $message = (string) $ffi->eggdb_last_error();
        $state = (string) $ffi->eggdb_last_sqlstate();
        throw new EggDbException($message !== '' ? $message : "EggDB status $status", $state, $status);
    }

    public function read(callable $work): mixed
    {
        $transaction = $this->begin(false);
        try {
            return $work($transaction);
        } finally {
            $transaction->rollback();
        }
    }

    public function write(callable $work): mixed
    {
        $transaction = $this->begin(true);
        try {
            $result = $work($transaction);
            $transaction->commit();
            return $result;
        } finally {
            $transaction->rollback();
        }
    }

    public function begin(bool $writing): Transaction
    {
        if ($this->database === null) {
            throw new EggDbException('the store is closed', '', 0);
        }
        $handle = $this->ffi->new('eggdb_txn*');
        $status = $writing
            ? $this->ffi->eggdb_begin_write($this->database, \FFI::addr($handle))
            : $this->ffi->eggdb_begin_read($this->database, \FFI::addr($handle));
        self::check($this->ffi, $status);
        return new Transaction($this->ffi, $handle, $writing);
    }

    public function close(): void
    {
        if ($this->database === null) {
            return;
        }
        $database = $this->database;
        $this->database = null;
        self::check($this->ffi, $this->ffi->eggdb_close($database));
    }
}
