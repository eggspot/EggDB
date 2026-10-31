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

}

#endif
