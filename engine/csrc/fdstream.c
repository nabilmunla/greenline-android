/* Open a PDF straight from a file descriptor (task 2bf.4).
 *
 * Android refuses to reopen an SAF fd through /proc/self/fd/N (seen on the phone, 25 Nov 2026), so
 * MuPDF reads the fd itself: a fz_stream whose next/seek callbacks use pread at the stream's own
 * position. Nothing is copied into memory beyond one 64 KB buffer, and the fd's own offset is never
 * moved. The fd stays owned by the caller (Rust's OwnedFd); the stream never closes it.
 *
 * Errors are caught here with fz_try (a MuPDF throw must never unwind through Rust frames) and
 * reported as NULL. The error text is dropped (fz_ignore_error): nothing here logs.
 *
 * Compiled by engine/build.rs against the headers of mupdf-sys 0.8.0 (MuPDF 1.27.2), the same way
 * mupdf-sys compiles its own wrapper (only -I mupdf/include, no defines).
 */

#include "mupdf/fitz.h"

#include <errno.h>
#include <unistd.h>

#define GL_FD_BUF (64 * 1024)

typedef struct {
	int fd;
	int64_t size;
	unsigned char buf[GL_FD_BUF];
} gl_fd_state;

static int gl_fd_next(fz_context *ctx, fz_stream *stm, size_t max)
{
	gl_fd_state *st = stm->state;
	ssize_t n;
	(void)max;
	do
		n = pread(st->fd, st->buf, sizeof st->buf, (off_t)stm->pos);
	while (n < 0 && errno == EINTR);
	if (n < 0)
		fz_throw(ctx, FZ_ERROR_SYSTEM, "pread failed");
	stm->rp = st->buf;
	stm->wp = st->buf + n;
	stm->pos += n;
	if (n == 0)
		return EOF;
	return *stm->rp++;
}

/* fz_seek turns SEEK_CUR into SEEK_SET before calling this, so whence is 0 or 2. */
static void gl_fd_seek(fz_context *ctx, fz_stream *stm, int64_t offset, int whence)
{
	gl_fd_state *st = stm->state;
	int64_t to = whence == SEEK_END ? st->size + offset : offset;
	/* Like MuPDF's own file stream: a seek past the end lands there and the next read is EOF
	 * (repair of a damaged file relies on it). Only a negative position is an error. */
	if (to < 0)
		fz_throw(ctx, FZ_ERROR_ARGUMENT, "seek before start");
	if (to > st->size)
		to = st->size;
	stm->pos = to;
	stm->rp = st->buf;
	stm->wp = st->buf;
}

static void gl_fd_drop(fz_context *ctx, void *state)
{
	fz_free(ctx, state);
}

/* Open `fd` (a seekable regular file of `size` bytes) as a PDF. NULL on any error. */
fz_document *gl_open_fd_document(fz_context *ctx, int fd, int64_t size)
{
	fz_stream *stm = NULL;
	fz_document *doc = NULL;
	fz_var(stm);
	fz_var(doc);
	fz_try(ctx)
	{
		gl_fd_state *st = fz_malloc_struct(ctx, gl_fd_state);
		st->fd = fd;
		st->size = size;
		/* fz_new_stream drops the state itself if it fails. */
		stm = fz_new_stream(ctx, st, gl_fd_next, gl_fd_drop);
		stm->seek = gl_fd_seek;
		doc = fz_open_document_with_stream(ctx, "application/pdf", stm);
	}
	fz_always(ctx)
	{
		/* The document keeps its own reference to the stream. */
		fz_drop_stream(ctx, stm);
	}
	fz_catch(ctx)
	{
		fz_ignore_error(ctx);
		return NULL;
	}
	return doc;
}
