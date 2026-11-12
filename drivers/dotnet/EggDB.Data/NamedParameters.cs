using System.Text;

namespace EggDB.Data;

internal static class NamedParameters
{
    internal static (string Text, IReadOnlyList<object?> Values) Positional(
        string text,
        IReadOnlyList<(string Name, object? Value)> parameters)
    {
        var byName = new Dictionary<string, object?>(StringComparer.OrdinalIgnoreCase);
        foreach (var (name, value) in parameters)
        {
            var bare = name.TrimStart('@', ':', '$');
            if (bare.Length > 0)
            {
                byName[bare] = value;
            }
        }
        var positional = parameters.Select(parameter => parameter.Value).ToList();
        if (byName.Count == 0)
        {
            return (text, positional);
        }
        var output = new StringBuilder(text.Length);
        var values = new List<object?>();
        var renamed = false;
        char? quote = null;
        for (var at = 0; at < text.Length; at++)
        {
            var character = text[at];
            if (quote is not null)
            {
                if (character == quote)
                {
                    quote = null;
                }
                output.Append(character);
                continue;
            }
            if (character is '\'' or '"')
            {
                quote = character;
                output.Append(character);
                continue;
            }
            var startsName = character is '@' or ':' or '$'
                && at + 1 < text.Length
                && (char.IsLetter(text[at + 1]) || text[at + 1] == '_')
                && !(character == ':' && at > 0 && text[at - 1] == ':');
            if (startsName)
            {
                var end = at + 1;
                while (end < text.Length && (char.IsLetterOrDigit(text[end]) || text[end] == '_'))
                {
                    end++;
                }
                var name = text[(at + 1)..end];
                if (byName.TryGetValue(name, out var value))
                {
                    output.Append('?');
                    values.Add(value);
                    renamed = true;
                    at = end - 1;
                    continue;
                }
            }
            output.Append(character);
        }
        return renamed ? (output.ToString(), values) : (text, positional);
    }
}
