using System;
using System.Collections.Generic;
using System.Globalization;
using System.Linq;
using Microsoft.Data.SqlClient;
using Microsoft.EntityFrameworkCore;

internal static class EfCoreAppCheck
{
    private static int Main(string[] arguments)
    {
        if (arguments.Length != 1)
        {
            Console.Error.WriteLine("usage: dotnet run --project EfCoreAppCheck -- <connection string>");
            return 2;
        }
        var connectionString = arguments[0];
        Step("migrate", () => Migrate(connectionString));
        Step("migrate again", () => MigrateAgain(connectionString));
        Step("insert", () => Insert(connectionString));
        Step("query", () => Query(connectionString));
        Step("update", () => Update(connectionString));
        Step("delete", () => Delete(connectionString));
        Step("transaction", () => RolledBackTransaction(connectionString));
        return 0;
    }

    private static void Step(string name, Action body)
    {
        Console.WriteLine("== " + name);
        try
        {
            body();
        }
        catch (SqlException failure)
        {
            PrintError(failure);
        }
        catch (DbUpdateException failure) when (failure.InnerException is SqlException inner)
        {
            PrintError(inner);
        }
    }

    private static void PrintError(SqlException failure)
    {
        Console.WriteLine("error " + Shown(failure.Number) + " " + failure.Message);
    }

    private static string Shown(object? value) => value switch
    {
        null => "NULL",
        decimal number => number.ToString(CultureInfo.InvariantCulture),
        bool flag => flag ? "1" : "0",
        DateTime moment => moment.ToString("yyyy-MM-dd HH:mm:ss", CultureInfo.InvariantCulture),
        IFormattable formattable => formattable.ToString(null, CultureInfo.InvariantCulture),
        _ => value.ToString() ?? "NULL",
    };

    private static string Row(params object?[] cells) => string.Join("|", cells.Select(Shown));

    private static void Migrate(string connectionString)
    {
        using var context = new ShopContext(connectionString);
        context.Database.Migrate();
        foreach (var migration in context.Database.GetAppliedMigrations())
        {
            Console.WriteLine("applied " + migration);
        }
    }

    private static void MigrateAgain(string connectionString)
    {
        using var context = new ShopContext(connectionString);
        context.Database.Migrate();
        Console.WriteLine("pending " + Shown(context.Database.GetPendingMigrations().Count()));
    }

    private static DateTime Moment(string text) =>
        DateTime.ParseExact(text, "yyyy-MM-dd HH:mm:ss", CultureInfo.InvariantCulture);

    private static Post NewPost(string title, int rating, string published, decimal price) => new Post
    {
        Title = title,
        Rating = rating,
        Published = Moment(published),
        Price = price,
    };

    private static void Insert(string connectionString)
    {
        using var context = new ShopContext(connectionString);
        var kitchen = new Blog
        {
            Name = "Kitchen",
            Url = "https://example.com/kitchen",
            Posts =
            {
                NewPost("Bread basics", 5, "2024-01-05 08:00:00", 12.50m),
                NewPost("Egg timing", 2, "2024-01-03 09:30:00", 0.35m),
                NewPost("Pie crust", 4, "2024-01-04 10:15:00", 7.25m),
                NewPost("Tart shells", 3, "2024-01-04 10:15:00", 4.20m),
                NewPost("Scone glaze", 1, "2024-01-06 07:45:00", 2.00m),
            },
        };
        var garden = new Blog
        {
            Name = "Garden",
            Posts = { NewPost("Herb pots", 4, "2024-01-02 06:00:00", 15.00m) },
        };
        context.Blogs.AddRange(kitchen, garden);
        Console.WriteLine("saved " + Shown(context.SaveChanges()));
        foreach (var blog in new[] { kitchen, garden })
        {
            Console.WriteLine(Row("blog", blog.Id, blog.Name));
            foreach (var post in blog.Posts)
            {
                Console.WriteLine(Row("post", post.Id, post.BlogId, post.Title));
            }
        }
    }

    private static void Query(string connectionString)
    {
        using var context = new ShopContext(connectionString);
        var minimumRating = 3;
        var page = context.Posts
            .Where(p => p.Rating >= minimumRating)
            .OrderBy(p => p.Published)
            .ThenBy(p => p.Id)
            .Skip(1)
            .Take(2)
            .Select(p => new { p.Id, p.Title, p.Published, BlogName = p.Blog.Name })
            .ToList();
        foreach (var post in page)
        {
            Console.WriteLine(Row("page", post.Id, post.Title, post.Published, post.BlogName));
        }

        var lowestPrice = 5m;
        Console.WriteLine("count " + Shown(context.Posts.Count(p => p.Price > lowestPrice)));
        Console.WriteLine("any Garden " + Shown(context.Blogs.Any(b => b.Name == "Garden")));
        Console.WriteLine("any Attic " + Shown(context.Blogs.Any(b => b.Name == "Attic")));

        var kitchen = context.Blogs
            .Include(b => b.Posts)
            .OrderBy(b => b.Id)
            .FirstOrDefault(b => b.Name == "Kitchen");
        if (kitchen is null)
        {
            Console.WriteLine("first none");
        }
        else
        {
            Console.WriteLine(Row("first", kitchen.Id, kitchen.Name, kitchen.Url, kitchen.Posts.Count));
            foreach (var post in kitchen.Posts.OrderBy(p => p.Id))
            {
                Console.WriteLine(Row("first post", post.Id, post.Title, post.Rating, post.Published, post.Price));
            }
        }

        var missingName = "Attic";
        var missing = context.Blogs.AsNoTracking().FirstOrDefault(b => b.Name == missingName);
        Console.WriteLine(missing is null ? "missing none" : Row("missing", missing.Id, missing.Name));

        var totals = context.Posts
            .GroupBy(p => p.BlogId)
            .Select(g => new { BlogId = g.Key, Posts = g.Count(), Total = g.Sum(p => p.Price) })
            .OrderBy(g => g.BlogId)
            .ToList();
        foreach (var total in totals)
        {
            Console.WriteLine(Row("group", total.BlogId, total.Posts, total.Total));
        }

        var chosen = new List<int> { 1, 3, 5 };
        var titles = context.Posts
            .Where(p => chosen.Contains(p.Id))
            .OrderBy(p => p.Id)
            .Select(p => p.Title)
            .ToList();
        Console.WriteLine("chosen " + string.Join(",", titles));
    }

    private static void PrintPosts(ShopContext context)
    {
        var posts = context.Posts.AsNoTracking().OrderBy(p => p.Id).ToList();
        foreach (var post in posts)
        {
            Console.WriteLine(Row("post", post.Id, post.BlogId, post.Title, post.Rating, post.Published, post.Price));
        }
    }

    private static void Update(string connectionString)
    {
        using var context = new ShopContext(connectionString);
        var egg = context.Posts.Single(p => p.Title == "Egg timing");
        var scone = context.Posts.Single(p => p.Title == "Scone glaze");
        var garden = context.Blogs.Single(b => b.Name == "Garden");
        egg.Rating = 4;
        scone.Price = 2.50m;
        garden.Url = "https://example.com/garden";
        Console.WriteLine("saved " + Shown(context.SaveChanges()));
        PrintPosts(context);
        foreach (var blog in context.Blogs.AsNoTracking().OrderBy(b => b.Id).ToList())
        {
            Console.WriteLine(Row("blog", blog.Id, blog.Name, blog.Url));
        }
    }

    private static void Delete(string connectionString)
    {
        using var context = new ShopContext(connectionString);
        var removed = context.Posts
            .Where(p => p.Title == "Tart shells" || p.Title == "Scone glaze")
            .ToList();
        context.Posts.RemoveRange(removed);
        Console.WriteLine("saved " + Shown(context.SaveChanges()));
        PrintPosts(context);
    }

    private static void RolledBackTransaction(string connectionString)
    {
        using var context = new ShopContext(connectionString);
        using (var transaction = context.Database.BeginTransaction())
        {
            var kitchen = context.Blogs.Single(b => b.Name == "Kitchen");
            kitchen.Posts.Add(NewPost("Rolled back", 5, "2024-02-01 12:00:00", 9.99m));
            Console.WriteLine("saved " + Shown(context.SaveChanges()));
            Console.WriteLine("inside " + Shown(context.Posts.Count()));
            transaction.Rollback();
        }
        Console.WriteLine("after " + Shown(context.Posts.Count()));
    }
}
