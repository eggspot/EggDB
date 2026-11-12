#ifndef EGGDB_HPP
#define EGGDB_HPP

#include <cstdint>
#include <optional>
#include <stdexcept>
#include <string>
#include <utility>
#include <variant>
#include <vector>

#include "eggdb.h"

namespace eggdb {

class error : public std::runtime_error {
public:
    error(int status, std::string sqlstate, const std::string &message)
        : std::runtime_error(message), status_(status), sqlstate_(std::move(sqlstate)) {}
    int status() const noexcept { return status_; }
    const std::string &sqlstate() const noexcept { return sqlstate_; }

private:
    int status_;
    std::string sqlstate_;
};

inline void check(int status) {
    if (status != EGGDB_OK) {
        const char *message = eggdb_last_error();
        const char *state = eggdb_last_sqlstate();
        throw error(status, state ? state : "", message ? message : "EggDB failed");
    }
}

using value = std::variant<std::monostate, std::int64_t, double, std::string, std::vector<std::uint8_t>>;

struct result {
    std::string tag;
    std::vector<std::string> columns;
    std::vector<std::vector<value>> rows;
};

class session {
public:
    static session open(const std::string &path, const std::string &dialect = "sqlite") {
        eggdb_options options{};
        eggdb_db *db = nullptr;
        check(eggdb_open(path.c_str(), &options, &db));
        eggdb_sql *sql = nullptr;
        int status = eggdb_sql_open(db, dialect.c_str(), &sql);
        if (status != EGGDB_OK) {
            eggdb_close(db);
            check(status);
        }
        return session(db, sql);
    }

    static session connect(const std::string &url) {
        eggdb_sql *sql = nullptr;
        check(eggdb_sql_connect(url.c_str(), &sql));
        return session(nullptr, sql);
    }

    session(session &&other) noexcept : db_(std::exchange(other.db_, nullptr)), sql_(std::exchange(other.sql_, nullptr)) {}
    session &operator=(session &&other) noexcept {
        if (this != &other) {
            close();
            db_ = std::exchange(other.db_, nullptr);
            sql_ = std::exchange(other.sql_, nullptr);
        }
        return *this;
    }
    session(const session &) = delete;
    session &operator=(const session &) = delete;
    ~session() { close(); }

    std::vector<result> execute(const std::string &text, const std::vector<std::optional<std::string>> &parameters = {}) {
        std::vector<eggdb_parameter> bound;
        bound.reserve(parameters.size());
        for (const auto &parameter : parameters) {
            eggdb_parameter one{};
            if (parameter) {
                one.text = reinterpret_cast<const std::uint8_t *>(parameter->data());
                one.len = parameter->size();
            }
            bound.push_back(one);
        }
        const eggdb_result *results = nullptr;
        std::size_t count = 0;
        check(eggdb_sql_execute_with_parameters(sql_, text.c_str(), bound.data(), bound.size(), &results, &count));
        std::vector<result> out;
        for (std::size_t index = 0; index < count; ++index) {
            const eggdb_result &one = results[index];
            result gathered;
            gathered.tag = one.tag ? one.tag : "";
            for (std::size_t column = 0; column < one.column_count; ++column) {
                gathered.columns.emplace_back(one.column_names[column]);
            }
            for (std::size_t row = 0; row < one.row_count; ++row) {
                std::vector<value> cells;
                for (std::size_t column = 0; column < one.column_count; ++column) {
                    const eggdb_value *cell = eggdb_result_value(&one, row, column);
                    switch (cell->kind) {
                        case EGGDB_INTEGER: cells.emplace_back(cell->integer); break;
                        case EGGDB_REAL: cells.emplace_back(cell->real); break;
                        case EGGDB_TEXT: cells.emplace_back(std::string(reinterpret_cast<const char *>(cell->data), cell->len)); break;
                        case EGGDB_BLOB: cells.emplace_back(std::vector<std::uint8_t>(cell->data, cell->data + cell->len)); break;
                        default: cells.emplace_back(std::monostate{}); break;
                    }
                }
                gathered.rows.push_back(std::move(cells));
            }
            out.push_back(std::move(gathered));
        }
        return out;
    }

    void close() noexcept {
        if (sql_) {
            eggdb_sql_close(sql_);
            sql_ = nullptr;
        }
        if (db_) {
            eggdb_close(db_);
            db_ = nullptr;
        }
    }

private:
    session(eggdb_db *db, eggdb_sql *sql) : db_(db), sql_(sql) {}
    eggdb_db *db_;
    eggdb_sql *sql_;
};

using bytes = std::vector<std::uint8_t>;

struct collection_stats {
    std::uint64_t entries = 0;
    std::uint64_t key_bytes = 0;
    std::uint64_t value_bytes = 0;
};

struct scan_options {
    std::optional<bytes> start;
    std::optional<bytes> end;
    std::optional<bytes> prefix;
    bool reverse = false;
    bool end_inclusive = false;
};

inline bytes to_bytes(const std::string &text) { return bytes(text.begin(), text.end()); }

class transaction {
public:
    transaction(transaction &&other) noexcept : txn_(std::exchange(other.txn_, nullptr)) {}
    transaction &operator=(transaction &&other) noexcept {
        if (this != &other) {
            rollback();
            txn_ = std::exchange(other.txn_, nullptr);
        }
        return *this;
    }
    transaction(const transaction &) = delete;
    transaction &operator=(const transaction &) = delete;
    ~transaction() { rollback(); }

    std::optional<bytes> get(const bytes &key, const char *collection = nullptr) {
        std::uint8_t *data = nullptr;
        std::size_t length = 0;
        check(eggdb_get(live(), collection, key.data(), key.size(), &data, &length));
        if (data == nullptr) {
            return std::nullopt;
        }
        bytes copied(data, data + length);
        eggdb_free(data, length);
        return copied;
    }

    void put(const bytes &key, const bytes &value, const char *collection = nullptr, std::uint64_t ttl_ms = 0) {
        if (ttl_ms > 0) {
            check(eggdb_put_with_expiry(live(), collection, key.data(), key.size(), value.data(), value.size(), ttl_ms));
        } else {
            check(eggdb_put(live(), collection, key.data(), key.size(), value.data(), value.size()));
        }
    }

    bool erase(const bytes &key, const char *collection = nullptr) {
        int deleted = 0;
        check(eggdb_delete(live(), collection, key.data(), key.size(), &deleted));
        return deleted != 0;
    }

    std::vector<std::pair<bytes, bytes>> scan(const scan_options &options = {}, const char *collection = nullptr) {
        std::uint32_t flags = (options.reverse ? EGGDB_SCAN_REVERSE : 0) | (options.end_inclusive ? EGGDB_SCAN_END_INCLUSIVE : 0);
        const bytes *start = options.start ? &*options.start : nullptr;
        const bytes *end = options.end ? &*options.end : nullptr;
        if (options.prefix) {
            flags |= EGGDB_SCAN_PREFIX;
            start = &*options.prefix;
            end = nullptr;
        }
        eggdb_scan *scan = nullptr;
        check(eggdb_scan_open(live(), collection, start ? start->data() : nullptr, start ? start->size() : 0,
                              end ? end->data() : nullptr, end ? end->size() : 0, flags, &scan));
        std::vector<std::pair<bytes, bytes>> found;
        for (;;) {
            const eggdb_entry *entries = nullptr;
            std::size_t count = 0;
            int status = eggdb_scan_next(live(), scan, 0, &entries, &count);
            if (status != EGGDB_OK) {
                eggdb_scan_free(scan);
                check(status);
            }
            if (count == 0) {
                break;
            }
            for (std::size_t index = 0; index < count; ++index) {
                found.emplace_back(bytes(entries[index].key, entries[index].key + entries[index].key_len),
                                   bytes(entries[index].value, entries[index].value + entries[index].value_len));
            }
        }
        eggdb_scan_free(scan);
        return found;
    }

    bool create_collection(const std::string &name) {
        int created = 0;
        check(eggdb_create_collection(live(), name.c_str(), &created));
        return created != 0;
    }

    bool drop_collection(const std::string &name) {
        int dropped = 0;
        check(eggdb_drop_collection(live(), name.c_str(), &dropped));
        return dropped != 0;
    }

    void rename_collection(const std::string &name, const std::string &new_name) {
        check(eggdb_rename_collection(live(), name.c_str(), new_name.c_str()));
    }

    std::vector<std::string> collections() {
        std::uint8_t *names = nullptr;
        std::size_t length = 0;
        std::size_t count = 0;
        check(eggdb_collection_names(live(), &names, &length, &count));
        std::vector<std::string> listed;
        std::size_t at = 0;
        while (listed.size() < count && at < length) {
            std::string name(reinterpret_cast<const char *>(names) + at);
            at += name.size() + 1;
            listed.push_back(std::move(name));
        }
        if (names != nullptr) {
            eggdb_free(names, length);
        }
        return listed;
    }

    collection_stats stats(const std::string &name) {
        eggdb_collection_stats counted{};
        check(eggdb_collection_stats(live(), name.c_str(), &counted));
        return collection_stats{counted.entries, counted.key_bytes, counted.value_bytes};
    }

    std::uint64_t commit() {
        std::uint64_t stamp = 0;
        check(eggdb_commit(std::exchange(txn_, nullptr), &stamp));
        return stamp;
    }

    void rollback() noexcept {
        if (txn_) {
            eggdb_rollback(std::exchange(txn_, nullptr));
        }
    }

private:
    friend class key_value;
    explicit transaction(eggdb_txn *txn) : txn_(txn) {}
    eggdb_txn *live() const {
        if (txn_ == nullptr) {
            throw error(EGGDB_ERR_INVALID_ARGUMENT, "", "the transaction has ended");
        }
        return txn_;
    }
    eggdb_txn *txn_;
};

class key_value {
public:
    static key_value open(const std::string &path, bool read_only = false) {
        eggdb_options options{};
        options.flags = read_only ? EGGDB_OPEN_READ_ONLY : 0;
        eggdb_db *db = nullptr;
        check(eggdb_open(path.c_str(), &options, &db));
        return key_value(db);
    }

    key_value(key_value &&other) noexcept : db_(std::exchange(other.db_, nullptr)) {}
    key_value &operator=(key_value &&other) noexcept {
        if (this != &other) {
            close();
            db_ = std::exchange(other.db_, nullptr);
        }
        return *this;
    }
    key_value(const key_value &) = delete;
    key_value &operator=(const key_value &) = delete;
    ~key_value() { close(); }

    transaction read() {
        eggdb_txn *txn = nullptr;
        check(eggdb_begin_read(db_, &txn));
        return transaction(txn);
    }

    transaction write() {
        eggdb_txn *txn = nullptr;
        check(eggdb_begin_write(db_, &txn));
        return transaction(txn);
    }

    void close() noexcept {
        if (db_) {
            eggdb_close(db_);
            db_ = nullptr;
        }
    }

private:
    explicit key_value(eggdb_db *db) : db_(db) {}
    eggdb_db *db_;
};

}

#endif
