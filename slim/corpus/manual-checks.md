# Manual interactive checks (not in the automated corpus)

The runner deliberately contains **no `-it` cases** — TTY allocation needs a
real terminal and can't be asserted reliably from a script. Run these by hand
against the daemon under test and eyeball the behavior:

1. **Interactive shell with TTY**

   ```sh
   docker run -it --rm alpine:3.19 sh
   ```

   Expect: a live prompt; `ls`/arrow keys/Ctrl-C work; resizing the terminal
   window updates `stty size` inside; `exit` (or Ctrl-D) returns to the host
   shell with the container removed.

2. **Exec with TTY into a running container**

   ```sh
   docker run -d --name slimtest-manual alpine:3.19 sleep 600
   docker exec -it slimtest-manual sh
   # ... poke around, then exit and:
   docker rm -f slimtest-manual
   ```

   Expect: interactive prompt inside the existing container; exiting the exec
   shell does NOT stop the container.

3. **Attach / detach keys**

   ```sh
   docker run -it --name slimtest-attach alpine:3.19 sh
   # detach with Ctrl-P Ctrl-Q (container keeps running), then:
   docker attach slimtest-attach
   # reattaches to the same shell; clean up with:
   docker rm -f slimtest-attach
   ```

   Expect: Ctrl-P Ctrl-Q detaches without killing the container; `attach`
   resumes the same session; input/output stay in sync after reattach.
