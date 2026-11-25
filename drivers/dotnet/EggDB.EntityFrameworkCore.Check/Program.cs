using EggDB.EntityFrameworkCore;
using Microsoft.EntityFrameworkCore;

var path = Path.Combine(Path.GetTempPath(), $"eggdb-efcore-check-{Environment.ProcessId}.eggdb");
File.Delete(path);
var connectionString = $"Data Source={path}";

using (var context = new ShopContext(connectionString))
{
    Check(context.Database.EnsureCreated(), "EnsureCreated makes the tables");
    var ada = new Author { Name = "Ada Byron" };
    context.Authors.Add(ada);
    context.Books.AddRange(
        new Book { Title = "Notes", Price = 19.90m, Published = new DateOnly(1843, 10, 1), InPrint = true, Author = ada },
        new Book { Title = "Sketch", Price = 9.99m, Published = new DateOnly(1842, 1, 1), InPrint = false, Author = ada });
    Check(context.SaveChanges() == 3, "three rows are saved");
    Check(ada.Id > 0, "the identity key comes back");
}

using (var context = new ShopContext(connectionString))
{
    var titles = context.Books.Where(book => book.InPrint && book.Price > 10m).OrderBy(book => book.Title).Select(book => book.Title).ToList();
    Check(titles.SequenceEqual(new[] { "Notes" }), "a filtered, ordered projection");
    var page = context.Books.OrderBy(book => book.Price).Skip(1).Take(1).Single();
    Check(page.Title == "Notes", "Skip and Take become OFFSET and LIMIT");
    var counts = context.Authors.Select(author => new { author.Name, Count = author.Books.Count }).Single();
    Check(counts.Count == 2 && counts.Name == "Ada Byron", "a join with a count");
    var book = context.Books.Single(book => book.Title == "Sketch");
    book.Price = 12.50m;
    context.SaveChanges();
    context.Books.Remove(context.Books.Single(book => book.Title == "Notes"));
    context.SaveChanges();
}

using (var context = new ShopContext(connectionString))
{
    Check(context.Books.Count() == 1 && context.Books.Single().Price == 12.50m, "updates and deletes are saved");
    Check(context.Database.EnsureDeleted(), "EnsureDeleted removes the file");
}

Console.WriteLine("EggDB EF Core provider check passed");
return 0;

static void Check(bool condition, string what)
{
    if (!condition)
    {
        throw new InvalidOperationException("failed: " + what);
    }
}

sealed class ShopContext : DbContext
{
    private readonly string _connectionString;

    public ShopContext(string connectionString) => _connectionString = connectionString;

    public DbSet<Author> Authors => Set<Author>();

    public DbSet<Book> Books => Set<Book>();

    protected override void OnConfiguring(DbContextOptionsBuilder optionsBuilder) => optionsBuilder.UseEggDb(_connectionString);
}

sealed class Author
{
    public int Id { get; set; }

    public string Name { get; set; } = string.Empty;

    public List<Book> Books { get; } = new();
}

sealed class Book
{
    public int Id { get; set; }

    public string Title { get; set; } = string.Empty;

    public decimal Price { get; set; }

    public DateOnly Published { get; set; }

    public bool InPrint { get; set; }

    public int AuthorId { get; set; }

    public Author? Author { get; set; }
}
