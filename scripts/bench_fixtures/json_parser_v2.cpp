#include <string>
#include <vector>
#include <unordered_map>
#include <variant>
#include <memory>
#include <sstream>
#include <charconv>
#include <optional>
#include <stdexcept>
#include <cstdint>
#include <string_view>
#include <cstddef>
#include <cmath>
#include <cctype>

namespace minijson {

/**
 * @brief Lexical token classes recognised by the scanner.
 * The Error variant is reserved for unrecoverable lexer faults so
 * the parser can bail out without throwing from inside the scan loop.
 */
enum class TokenType {
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Colon,
    Comma,
    String,
    Number,
    True,
    False,
    Null,
    Eof,
    Error,
};

/**
 * @brief Categorises a parse failure for downstream reporting.
 * Paired with the source position carried by ParseError to produce
 * a human-readable diagnostic. Keep variants in sync with set_error().
 */
enum class ParseErrorKind {
    UnexpectedChar,
    UnterminatedString,
    InvalidNumber,
    InvalidEscape,
    Overflow,
    MaxDepth,
    TrailingGarbage,
};

/**
 * @brief A single lexical token produced by the scanner.
 * Tokens own their lexeme so the parser can outlive the input buffer
 * (e.g. when parsing into an AST that escapes the source string_view).
 */
struct Token {
    TokenType type;
    std::string lexeme;
    std::size_t line;
    std::size_t col;
};

class JsonValue;

using JsonArray  = std::vector<JsonValue>;
using JsonObject = std::unordered_map<std::string, JsonValue>;

/**
 * @brief Tagged union describing every node of a parsed JSON document.
 * Accessors throw std::bad_variant_access on type mismatch; use the
 * is_*() probes before calling as_*() if the schema is dynamic.
 */
class JsonValue {
public:
    using Storage = std::variant<
        std::nullptr_t,
        bool,
        std::int64_t,
        double,
        std::string,
        JsonArray,
        JsonObject>;

    /// Default-constructs a null value.
    JsonValue() : data_(nullptr) {}

    /// Constructs a value from any compatible payload.
    template <typename T>
    JsonValue(T&& v) : data_(std::forward<T>(v)) {}

    bool is_null()   const { return std::holds_alternative<std::nullptr_t>(data_); }
    bool is_object() const { return std::holds_alternative<JsonObject>(data_); }
    bool is_array()  const { return std::holds_alternative<JsonArray>(data_); }
    bool is_string() const { return std::holds_alternative<std::string>(data_); }
    bool is_int()    const { return std::holds_alternative<std::int64_t>(data_); }
    bool is_double() const { return std::holds_alternative<double>(data_); }
    bool is_bool()   const { return std::holds_alternative<bool>(data_); }

    /**
     * @brief Borrow the underlying string payload.
     * @return const reference to the stored string.
     * @throws std::bad_variant_access if the value is not a string.
     */
    const std::string& as_string() const {
        return std::get<std::string>(data_);
    }

    /**
     * @brief Read the value as a 64-bit signed integer.
     * @return the stored integer.
     * @throws std::bad_variant_access if the value is not an int.
     */
    std::int64_t as_int() const {
        return std::get<std::int64_t>(data_);
    }

    /**
     * @brief Read the value as a double, widening from int when needed.
     * @return the stored double, or the int payload converted to double.
     * @throws std::bad_variant_access if the value is neither numeric.
     */
    double as_double() const {
        if (std::holds_alternative<double>(data_)) {
            return std::get<double>(data_);
        }
        return static_cast<double>(std::get<std::int64_t>(data_));
    }

    /// @return the underlying boolean. @throws on type mismatch.
    bool as_bool() const {
        return std::get<bool>(data_);
    }

    /// @return mutable access to the array payload.
    JsonArray& as_array() { return std::get<JsonArray>(data_); }

    /// @return mutable access to the object payload.
    JsonObject& as_object() { return std::get<JsonObject>(data_); }

    /// @return const access to the array payload.
    const JsonArray& as_array() const { return std::get<JsonArray>(data_); }

    /// @return const access to the object payload.
    const JsonObject& as_object() const { return std::get<JsonObject>(data_); }

    /// @return raw access to the underlying variant.
    const Storage& storage() const { return data_; }

private:
    Storage data_;
};

/**
 * @brief Exception thrown by JsonParser on unrecoverable parse faults.
 * Carries a structured ParseErrorKind alongside 1-based line/column
 * so callers can surface IDE-style diagnostics without re-parsing.
 */
class ParseError : public std::runtime_error {
public:
    /**
     * @brief Construct a fully-described parse error.
     * @param kind structured failure category.
     * @param msg  human-readable narrative.
     * @param line 1-based line number where the fault was detected.
     * @param col  1-based column number where the fault was detected.
     */
    ParseError(ParseErrorKind kind, const std::string& msg,
               std::size_t line, std::size_t col)
        : std::runtime_error(msg), kind_(kind), line_(line), col_(col) {}

    /// @return structured kind of this error.
    ParseErrorKind kind() const noexcept { return kind_; }

    /// @return 1-based source line.
    std::size_t line() const noexcept { return line_; }

    /// @return 1-based source column.
    std::size_t col() const noexcept { return col_; }

private:
    ParseErrorKind kind_;
    std::size_t line_;
    std::size_t col_;
};

/**
 * @brief Recursive-descent JSON parser; not thread-safe.
 * Re-using the same instance across calls to parse() is supported
 * and slightly cheaper than constructing a fresh one each time.
 */
class JsonParser {
public:
    /// Maximum nesting depth before MaxDepth is raised.
    static constexpr std::size_t kDefaultMaxDepth = 128;
    /// Hard ceiling enforced even when callers request a larger limit.
    static constexpr std::size_t kAbsoluteMaxDepth = 1024;

    JsonParser() = default;

    /**
     * @brief Parse a complete JSON document.
     * @param src input bytes; must be valid UTF-8.
     * @return the root JsonValue.
     * @throws ParseError on any lexical or syntactic fault.
     */
    JsonValue parse(std::string_view src) {
        src_   = src;
        pos_   = 0;
        line_  = 1;
        col_   = 1;
        depth_ = 0;
        peeked_.reset();
        JsonValue root = parse_value();
        Token tail = advance();
        if (tail.type != TokenType::Eof) {
            set_error(ParseErrorKind::TrailingGarbage,
                      "trailing content after document", tail.line, tail.col);
        }
        return root;
    }

    /**
     * @brief Parse a document with a caller-supplied depth limit.
     * @param src input bytes; must be valid UTF-8.
     * @param max maximum nesting depth; clamped to kAbsoluteMaxDepth.
     * @return the root JsonValue.
     * @throws ParseError on any lexical or syntactic fault.
     */
    JsonValue parse_with_max_depth(std::string_view src, std::size_t max) {
        std::size_t prev = max_depth_;
        max_depth_ = std::min(max, kAbsoluteMaxDepth);
        try {
            JsonValue v = parse(src);
            max_depth_ = prev;
            return v;
        } catch (...) {
            max_depth_ = prev;
            throw;
        }
    }

    /**
     * @brief Parse the next value at the current position.
     * @return the parsed JsonValue.
     * @throws ParseError if the stream does not start a valid value.
     */
    JsonValue parse_value() {
        if (++depth_ > max_depth_) {
            set_error(ParseErrorKind::MaxDepth,
                      "maximum nesting depth exceeded", line_, col_);
        }
        if (!peeked_.has_value()) peeked_ = scan();
        Token t = *peeked_;
        JsonValue out;
        switch (t.type) {
            case TokenType::LBrace:   out = parse_object(); break;
            case TokenType::LBracket: out = parse_array();  break;
            case TokenType::String:   advance(); out = JsonValue(std::move(t.lexeme)); break;
            case TokenType::Number:   out = parse_number(); break;
            case TokenType::True:     advance(); out = JsonValue(true);  break;
            case TokenType::False:    advance(); out = JsonValue(false); break;
            case TokenType::Null:     advance(); out = JsonValue(nullptr); break;
            default:
                set_error(ParseErrorKind::UnexpectedChar,
                          "expected a JSON value", t.line, t.col);
        }
        --depth_;
        return out;
    }

    /**
     * @brief Parse an object body, assuming the leading '{' is next.
     * @return the populated JsonObject wrapped in a JsonValue.
     * @throws ParseError on malformed key/value pairs.
     */
    JsonValue parse_object() {
        expect(TokenType::LBrace);
        JsonObject obj;
        if (!peeked_.has_value()) peeked_ = scan();
        if (peeked_->type == TokenType::RBrace) {
            advance();
            return JsonValue(std::move(obj));
        }
        while (true) {
            Token key = advance();
            if (key.type != TokenType::String) {
                set_error(ParseErrorKind::UnexpectedChar,
                          "expected string key", key.line, key.col);
            }
            expect(TokenType::Colon);
            obj.emplace(std::move(key.lexeme), parse_value());
            Token sep = advance();
            if (sep.type == TokenType::RBrace) break;
            if (sep.type != TokenType::Comma) {
                set_error(ParseErrorKind::UnexpectedChar,
                          "expected ',' or '}'", sep.line, sep.col);
            }
        }
        return JsonValue(std::move(obj));
    }

    /**
     * @brief Parse an array body, assuming the leading '[' is next.
     * @return the populated JsonArray wrapped in a JsonValue.
     * @throws ParseError on malformed elements or separators.
     */
    JsonValue parse_array() {
        expect(TokenType::LBracket);
        JsonArray arr;
        if (!peeked_.has_value()) peeked_ = scan();
        if (peeked_->type == TokenType::RBracket) {
            advance();
            return JsonValue(std::move(arr));
        }
        while (true) {
            arr.push_back(parse_value());
            Token sep = advance();
            if (sep.type == TokenType::RBracket) break;
            if (sep.type != TokenType::Comma) {
                set_error(ParseErrorKind::UnexpectedChar,
                          "expected ',' or ']'", sep.line, sep.col);
            }
        }
        return JsonValue(std::move(arr));
    }

    /**
     * @brief Decode the current string token into a UTF-8 std::string.
     * @return the decoded string value.
     * @throws ParseError on bad escapes or unterminated literals.
     */
    std::string parse_string() {
        Token t = advance();
        if (t.type != TokenType::String) {
            set_error(ParseErrorKind::UnexpectedChar,
                      "expected string", t.line, t.col);
        }
        return std::move(t.lexeme);
    }

    /**
     * @brief Convert the current numeric token into int64 or double.
     * @return a JsonValue holding either int64_t or double.
     * @throws ParseError on overflow or syntactically invalid digits.
     */
    JsonValue parse_number() {
        Token t = advance();
        const std::string& s = t.lexeme;
        if (s.empty()) {
            set_error(ParseErrorKind::InvalidNumber,
                      "empty numeric literal", t.line, t.col);
        }
        if (s.size() >= 2 && s[0] == '0' && std::isdigit(static_cast<unsigned char>(s[1]))) {
            set_error(ParseErrorKind::InvalidNumber,
                      "leading zero in integer literal", t.line, t.col);
        }
        if (s.size() >= 3 && s[0] == '-' && s[1] == '0' && std::isdigit(static_cast<unsigned char>(s[2]))) {
            set_error(ParseErrorKind::InvalidNumber,
                      "leading zero after sign", t.line, t.col);
        }
        bool is_float = s.find_first_of(".eE") != std::string::npos;
        if (is_float) {
            try {
                double d = std::stod(s);
                if (std::isinf(d)) {
                    set_error(ParseErrorKind::Overflow,
                              "double literal evaluated to infinity", t.line, t.col);
                }
                if (std::isnan(d)) {
                    set_error(ParseErrorKind::InvalidNumber,
                              "double literal evaluated to NaN", t.line, t.col);
                }
                return JsonValue(d);
            } catch (const std::out_of_range&) {
                set_error(ParseErrorKind::Overflow,
                          "double literal out of range", t.line, t.col);
            } catch (const std::invalid_argument&) {
                set_error(ParseErrorKind::InvalidNumber,
                          "invalid double literal", t.line, t.col);
            }
        }
        std::int64_t v = 0;
        auto first = s.data();
        auto last  = s.data() + s.size();
        auto res   = std::from_chars(first, last, v);
        if (res.ec == std::errc::result_out_of_range) {
            set_error(ParseErrorKind::Overflow,
                      "integer literal out of range", t.line, t.col);
        }
        if (res.ec != std::errc() || res.ptr != last) {
            set_error(ParseErrorKind::InvalidNumber,
                      "invalid integer literal", t.line, t.col);
        }
        return JsonValue(v);
    }

    /**
     * @brief Match a fixed keyword such as "true", "false", or "null".
     * @param expected the literal to match against the input.
     * @param tt       the token type to emit on success.
     * @return a Token carrying the matched lexeme.
     */
    Token parse_literal(std::string_view expected, TokenType tt) {
        std::size_t start_line = line_;
        std::size_t start_col  = col_;
        for (char c : expected) {
            if (pos_ >= src_.size() || src_[pos_] != c) {
                set_error(ParseErrorKind::UnexpectedChar,
                          "invalid literal", line_, col_);
            }
            ++pos_;
            ++col_;
        }
        return Token{tt, std::string(expected), start_line, start_col};
    }

    /**
     * @brief Consume and return the next token.
     * @return the next token in the input.
     */
    Token advance() {
        if (peeked_.has_value()) {
            Token t = std::move(*peeked_);
            peeked_.reset();
            return t;
        }
        return scan();
    }

    /**
     * @brief Consume the next token and assert it has the given type.
     * @param tt the token type that must match.
     * @throws ParseError if the actual type does not match @p tt.
     */
    void expect(TokenType tt) {
        Token t = advance();
        if (t.type != tt) {
            set_error(ParseErrorKind::UnexpectedChar,
                      "unexpected token", t.line, t.col);
        }
    }

private:
    /**
     * @brief Raise a ParseError with a structured kind and source position.
     * @param kind the structured failure category.
     * @param msg  the human-readable narrative.
     * @param line the 1-based line number of the offending byte.
     * @param col  the 1-based column of the offending byte.
     */
    [[noreturn]] void set_error(ParseErrorKind kind, const std::string& msg,
                                std::size_t line, std::size_t col) {
        std::ostringstream os;
        os << msg << " at " << line << ":" << col;
        throw ParseError(kind, os.str(), line, col);
    }

    Token scan() {
        skip_ws();
        if (pos_ >= src_.size()) {
            return Token{TokenType::Eof, "", line_, col_};
        }
        std::size_t sl = line_;
        std::size_t sc = col_;
        char c = src_[pos_];
        switch (c) {
            case '{': ++pos_; ++col_; return Token{TokenType::LBrace,   "{", sl, sc};
            case '}': ++pos_; ++col_; return Token{TokenType::RBrace,   "}", sl, sc};
            case '[': ++pos_; ++col_; return Token{TokenType::LBracket, "[", sl, sc};
            case ']': ++pos_; ++col_; return Token{TokenType::RBracket, "]", sl, sc};
            case ':': ++pos_; ++col_; return Token{TokenType::Colon,    ":", sl, sc};
            case ',': ++pos_; ++col_; return Token{TokenType::Comma,    ",", sl, sc};
            case 't': return parse_literal("true",  TokenType::True);
            case 'f': return parse_literal("false", TokenType::False);
            case 'n': return parse_literal("null",  TokenType::Null);
            case '"': return scan_string();
            default:
                if (c == '-' || (c >= '0' && c <= '9')) {
                    return scan_number();
                }
                set_error(ParseErrorKind::UnexpectedChar,
                          "unexpected character", line_, col_);
        }
    }

    Token scan_string() {
        std::size_t sl = line_, sc = col_;
        ++pos_; ++col_;
        std::string out;
        while (pos_ < src_.size()) {
            char c = src_[pos_];
            if (c == '"') { ++pos_; ++col_; return Token{TokenType::String, std::move(out), sl, sc}; }
            if (c == '\\') {
                ++pos_; ++col_;
                if (pos_ >= src_.size()) break;
                char e = src_[pos_++]; ++col_;
                switch (e) {
                    case '"':  out.push_back('"');  break;
                    case '\\': out.push_back('\\'); break;
                    case '/':  out.push_back('/');  break;
                    case 'b':  out.push_back('\b'); break;
                    case 'f':  out.push_back('\f'); break;
                    case 'n':  out.push_back('\n'); break;
                    case 'r':  out.push_back('\r'); break;
                    case 't':  out.push_back('\t'); break;
                    case 'u': {
                        if (pos_ + 4 > src_.size()) set_error(ParseErrorKind::InvalidEscape, "truncated \\u escape", line_, col_);
                        unsigned cp = 0;
                        for (int k = 0; k < 4; ++k) {
                            char h = src_[pos_++]; ++col_; cp <<= 4;
                            if (h >= '0' && h <= '9') cp |= unsigned(h - '0');
                            else if (h >= 'a' && h <= 'f') cp |= unsigned(h - 'a' + 10);
                            else if (h >= 'A' && h <= 'F') cp |= unsigned(h - 'A' + 10);
                            else set_error(ParseErrorKind::InvalidEscape, "non-hex digit in \\u escape", line_, col_);
                        }
                        if (cp < 0x80) out.push_back(static_cast<char>(cp));
                        else if (cp < 0x800) { out.push_back(static_cast<char>(0xC0 | (cp >> 6))); out.push_back(static_cast<char>(0x80 | (cp & 0x3F))); }
                        else { out.push_back(static_cast<char>(0xE0 | (cp >> 12))); out.push_back(static_cast<char>(0x80 | ((cp >> 6) & 0x3F))); out.push_back(static_cast<char>(0x80 | (cp & 0x3F))); }
                        break;
                    }
                    default:
                        set_error(ParseErrorKind::InvalidEscape, "unknown escape", line_, col_);
                }
            } else if (c == '\n') {
                set_error(ParseErrorKind::UnterminatedString, "newline in string literal", line_, col_);
            } else {
                out.push_back(c); ++pos_; ++col_;
            }
        }
        set_error(ParseErrorKind::UnterminatedString, "unterminated string literal", sl, sc);
    }

    Token scan_number() {
        std::size_t sl = line_, sc = col_, start = pos_;
        if (src_[pos_] == '-') { ++pos_; ++col_; }
        while (pos_ < src_.size() && std::isdigit(static_cast<unsigned char>(src_[pos_]))) { ++pos_; ++col_; }
        if (pos_ < src_.size() && src_[pos_] == '.') {
            ++pos_; ++col_;
            while (pos_ < src_.size() && std::isdigit(static_cast<unsigned char>(src_[pos_]))) { ++pos_; ++col_; }
        }
        if (pos_ < src_.size() && (src_[pos_] == 'e' || src_[pos_] == 'E')) {
            ++pos_; ++col_;
            if (pos_ < src_.size() && (src_[pos_] == '+' || src_[pos_] == '-')) { ++pos_; ++col_; }
            while (pos_ < src_.size() && std::isdigit(static_cast<unsigned char>(src_[pos_]))) { ++pos_; ++col_; }
        }
        return Token{TokenType::Number, std::string(src_.substr(start, pos_ - start)), sl, sc};
    }

    void skip_ws() {
        while (pos_ < src_.size()) {
            char c = src_[pos_];
            if (c == ' ' || c == '\t' || c == '\r') { ++pos_; ++col_; }
            else if (c == '\n') { ++pos_; ++line_; col_ = 1; }
            else break;
        }
    }

    std::string_view src_;
    std::size_t pos_   = 0;
    std::size_t line_  = 1;
    std::size_t col_   = 1;
    std::size_t depth_ = 0;
    std::size_t max_depth_ = kDefaultMaxDepth;
    std::optional<Token> peeked_;
};

/**
 * @brief Coerce a JsonValue into a typed C++ scalar.
 * @tparam T target type; supports int, double, std::string, bool.
 * @param  v the source value.
 * @return populated optional on success, std::nullopt on type mismatch.
 */
template <typename T>
std::optional<T> coerce(const JsonValue& v);

template <>
std::optional<int> coerce<int>(const JsonValue& v) {
    if (v.is_int()) {
        std::int64_t raw = v.as_int();
        if (raw < std::numeric_limits<int>::min() ||
            raw > std::numeric_limits<int>::max()) {
            return std::nullopt;
        }
        return static_cast<int>(raw);
    }
    if (v.is_double()) {
        double d = v.as_double();
        if (std::trunc(d) != d) return std::nullopt;
        return static_cast<int>(d);
    }
    return std::nullopt;
}

template <>
std::optional<double> coerce<double>(const JsonValue& v) {
    if (v.is_double() || v.is_int()) {
        return v.as_double();
    }
    return std::nullopt;
}

template <>
std::optional<std::string> coerce<std::string>(const JsonValue& v) {
    if (v.is_string()) return v.as_string();
    return std::nullopt;
}

template <>
std::optional<bool> coerce<bool>(const JsonValue& v) {
    if (v.is_bool()) return v.as_bool();
    return std::nullopt;
}

/**
 * @brief Escape a raw string into a JSON-safe quoted literal.
 * @param in the input bytes; assumed to be UTF-8.
 * @return a quoted, escaped representation suitable for emission.
 */
std::string escape_string(std::string_view in) {
    std::string out;
    out.reserve(in.size() + 2);
    out.push_back('"');
    std::size_t i = 0;
    while (i < in.size()) {
        unsigned char b0 = static_cast<unsigned char>(in[i]);
        if (b0 == '"') { out += "\\\""; ++i; continue; }
        if (b0 == '\\') { out += "\\\\"; ++i; continue; }
        if (b0 == '/') { out += "\\/"; ++i; continue; }
        if (b0 == '\b') { out += "\\b";  ++i; continue; }
        if (b0 == '\f') { out += "\\f";  ++i; continue; }
        if (b0 == '\n') { out += "\\n";  ++i; continue; }
        if (b0 == '\r') { out += "\\r";  ++i; continue; }
        if (b0 == '\t') { out += "\\t";  ++i; continue; }
        if (b0 < 0x20) {
            char buf[8];
            std::snprintf(buf, sizeof(buf), "\\u%04x", b0);
            out += buf;
            ++i;
            continue;
        }
        if (b0 < 0x80) {
            out.push_back(static_cast<char>(b0));
            ++i;
            continue;
        }
        std::size_t need = 0;
        if      ((b0 & 0xE0) == 0xC0) need = 1;
        else if ((b0 & 0xF0) == 0xE0) need = 2;
        else if ((b0 & 0xF8) == 0xF0) need = 3;
        else {
            out += "\\ufffd";
            ++i;
            continue;
        }
        if (i + need >= in.size()) {
            out += "\\ufffd";
            ++i;
            continue;
        }
        out.push_back(static_cast<char>(b0));
        for (std::size_t k = 1; k <= need; ++k) {
            out.push_back(in[i + k]);
        }
        i += need + 1;
    }
    out.push_back('"');
    return out;
}

/**
 * @brief Serialise a JsonValue back to a compact JSON string.
 * @param v the value to emit.
 * @return a UTF-8 std::string containing valid JSON.
 */
std::string to_string(const JsonValue& v) {
    std::ostringstream os;
    if (v.is_null()) {
        os << "null";
    } else if (v.is_bool()) {
        os << (v.as_bool() ? "true" : "false");
    } else if (v.is_int()) {
        os << v.as_int();
    } else if (v.is_double()) {
        os << v.as_double();
    } else if (v.is_string()) {
        os << escape_string(v.as_string());
    } else if (v.is_array()) {
        os << '[';
        bool first = true;
        for (const auto& e : v.as_array()) {
            if (!first) os << ',';
            os << to_string(e);
            first = false;
        }
        os << ']';
    } else if (v.is_object()) {
        os << '{';
        bool first = true;
        for (const auto& kv : v.as_object()) {
            if (!first) os << ',';
            os << escape_string(kv.first) << ':' << to_string(kv.second);
            first = false;
        }
        os << '}';
    }
    return os.str();
}

} // namespace minijson
