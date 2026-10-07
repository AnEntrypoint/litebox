#include <stdio.h>
#include <dlfcn.h>

typedef struct _GSList GSList;
struct _GSList {
    void *data;
    GSList *next;
};

typedef struct _GdkPixbufFormat GdkPixbufFormat;
typedef int gboolean;
typedef unsigned int guint;

static guint slist_length(GSList *l) {
    guint n = 0;
    while (l) { n++; l = l->next; }
    return n;
}

int main(void) {
    void *lib = dlopen("libgdk_pixbuf-2.0.so.0", RTLD_NOW);
    if (!lib) {
        printf("DLOPEN_FAILED err=%s\n", dlerror());
        return 1;
    }
    printf("DLOPEN_OK\n");

    GSList *(*get_formats)(void) = dlsym(lib, "gdk_pixbuf_get_formats");
    char *(*format_get_name)(GdkPixbufFormat *) = dlsym(lib, "gdk_pixbuf_format_get_name");
    char *(*format_get_desc)(GdkPixbufFormat *) = dlsym(lib, "gdk_pixbuf_format_get_description");
    gboolean (*format_is_disabled)(GdkPixbufFormat *) = dlsym(lib, "gdk_pixbuf_format_is_disabled");
    void (*g_free_fn)(void *) = dlsym(lib, "g_free");

    if (!get_formats || !format_get_name || !g_free_fn) {
        printf("DLSYM_FAILED get_formats=%p name=%p free=%p err=%s\n",
               (void *)get_formats, (void *)format_get_name, (void *)g_free_fn, dlerror());
        return 1;
    }
    printf("DLSYM_OK\n");

    GSList *formats = get_formats();
    guint count = slist_length(formats);
    printf("FORMAT_COUNT=%u\n", count);
    for (GSList *l = formats; l != NULL; l = l->next) {
        GdkPixbufFormat *fmt = (GdkPixbufFormat *)l->data;
        char *name = format_get_name(fmt);
        char *desc = format_get_desc ? format_get_desc(fmt) : NULL;
        gboolean disabled = format_is_disabled ? format_is_disabled(fmt) : -1;
        printf("FORMAT name=%s desc=%s disabled=%d\n", name ? name : "(null)", desc ? desc : "(null)", disabled);
        if (name) g_free_fn(name);
        if (desc) g_free_fn(desc);
    }
    printf("DONE\n");
    return 0;
}
