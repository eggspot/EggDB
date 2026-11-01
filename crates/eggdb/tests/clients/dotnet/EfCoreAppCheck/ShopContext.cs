using System;
using System.Collections.Generic;
using Microsoft.EntityFrameworkCore;

public sealed class Blog
{
    public int Id { get; set; }

    public string Name { get; set; } = "";

    public string? Url { get; set; }

    public List<Post> Posts { get; set; } = new List<Post>();
}

public sealed class Post
{
    public int Id { get; set; }

    public string Title { get; set; } = "";

    public int Rating { get; set; }

    public DateTime Published { get; set; }

    public decimal Price { get; set; }

    public int BlogId { get; set; }

    public Blog Blog { get; set; } = null!;
}

public sealed class ShopContext : DbContext
{
    private readonly string connectionString;

    public ShopContext(string connectionString)
    {
        this.connectionString = connectionString;
    }

    public DbSet<Blog> Blogs => Set<Blog>();

    public DbSet<Post> Posts => Set<Post>();

    protected override void OnConfiguring(DbContextOptionsBuilder optionsBuilder)
    {
        optionsBuilder.UseSqlServer(connectionString);
    }

    protected override void OnModelCreating(ModelBuilder modelBuilder)
    {
        modelBuilder.Entity<Blog>(blog =>
        {
            blog.Property(b => b.Name).IsRequired().HasMaxLength(100);
            blog.HasMany(b => b.Posts)
                .WithOne(p => p.Blog)
                .HasForeignKey(p => p.BlogId)
                .OnDelete(DeleteBehavior.Cascade);
        });
        modelBuilder.Entity<Post>(post =>
        {
            post.Property(p => p.Title).IsRequired().HasMaxLength(200);
            post.Property(p => p.Price).HasPrecision(10, 2);
        });
    }
}
