// Copyright 2017 Google Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License"); you may not
// use this file except in compliance with the License.  You may obtain a copy
// of the License at:
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS, WITHOUT
// WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.  See the
// License for the specific language governing permissions and limitations
// under the License.

// The sandboxfs binary mounts an instance of the sandboxfs filesystem.
package main

import (
	"flag"
	"fmt"
	"io"
	"io/ioutil"
	"log"
	"os"
	"os/signal"
	"path/filepath"
	"runtime"
	"syscall"
	"time"

	"bazil.org/fuse"
	"github.com/bazelbuild/sandboxfs/internal/sandbox"
)

var (
	// packageVersion contains the textual version number of this package.
	//
	// The value here is populated by the build system.  The placeholder denotes that
	// a build that misses to include this detail should not be shipped to users.
	packageVersion = "0.0 (BINARY NOT FOR RELEASE)"
)

// handleSignals installs signal handlers to ensure the file system is unmounted.
//
// The signal handler is responsible for unmounting the file system, which in turn causes the
// FUSE serve loop to either never start or to finish execution if it was already running.
//
// But this is tricky business because of a potential race: if the signal handler is installed
// and a signal arrives *before* the mount point has been configured, the unmounting will not
// succeed, which means we will enter the server loop and lose the signal. Conversely, if we
// did this backwards, we could receive a signal after the mount point has been configured but
// before we install the signal handler, which means we'd terminate but leak the mount point.
//
// To solve this, we must install the signal handler first, but we must not take action on
// signals until after we know that the mount point has been set up. This is what the mountPoint
// channel is for: the caller must inject the mount point into this handler only after the mount
// operation has succeeded, or an empty string if the mount operation failed.
//
// The caller can know if a signal was the cause of the FUSE serve loop termination by inspecting
// the contents of the caughtSignal output channel.
//
// NOTE: This would be much easier if we could just mask signals while we prepare the mount point in
// the caller and then process them once we are ready. Unfortunately, signal.Ignore() does not only
// mask signals: it discards them as well! The x/sys/unix package is supposed to gain signal
// handling at some point so maybe we'll be able to revisit this in the future? Who knows.
func handleSignals(mountPoint <-chan string, caughtSignal chan<- os.Signal) {
	handler := make(chan os.Signal, 1)
	signal.Notify(handler, syscall.SIGHUP, os.Interrupt, syscall.SIGQUIT, syscall.SIGTERM)

	go func() {
		caughtSignal <- <-handler // Wait for the signal.

		// Wait until the main process has had a chance to issue the mount(2) system call.
		// It doesn't matter if such call succeeded or not: we must wait anyway before
		// attempting to do any cleanup.
		mnt := <-mountPoint

		if mnt != "" {
			// Now the real deal: the main process did actually get to issue a
			// successful mount(2) system call. Make the mount point vanish so that the
			// FUSE serve loop terminates or prevent it from starting.
			backoff := 10 * time.Millisecond
			for {
				err := fuse.Unmount(mnt)
				if err == nil {
					break
				}

				// If unmounting fails, it is probably because the file system is
				// busy. We don't know, but it doesn't matter: we have entered a
				// terminal status: we know we have to exit, so we'll keep trying to
				// unclog things while telling the user what's going on.  They are
				// the ones that have to fix this situation.
				log.Printf("unmounting filesystem failed with error: %v; will retry in %v", err, backoff)
				time.Sleep(backoff)
				if backoff < time.Second {
					backoff = backoff * 2
				}
			}
		}
	}()
}

func serve(settings ProfileSettings, mountPoint string, options []fuse.MountOption, initialMappings []sandbox.MappingSpec, reconfigInput io.Reader, reconfigOutput io.Writer) error {
	root, err := sandbox.CreateRoot(nil, initialMappings)
	if err != nil {
		return fmt.Errorf("unable to init sandbox: %v", err)
	}

	profileContext, err := StartProfiling(settings)
	if err != nil {
		return err
	}
	defer profileContext.Close()

	// OSXFUSE unconditionally creates the mount point if it does not exist while Linux's FUSE
	// errors out on this condition. Linux is behaving correctly here, but to unify the behavior
	// between the two cases (and, especially, to ensure that the error message that we print is
	// consistent), explicitly test for the mount point's existence.
	//
	// Note that this is knowingly racy. If the mount point is created after this call but before
	// the actual mount operation happens, we'll be subject to OS-specific behavior. We cannot do
	// do better, and this is not a big deal anyway.
	//
	// TODO(jmmv): Fix OSXFUSE to not create the mount point. If that's undesirable upstream, an
	// alternative would be to add a "-o nocreate_mount" option to mount_osxfuse and then use that
	// in the fuse.Mount call below.
	if _, err := os.Lstat(mountPoint); os.IsNotExist(err) {
		return fmt.Errorf("unable to mount: %s does not exist", mountPoint)
	}

	mountOk := make(chan string, 1)
	caughtSignal := make(chan os.Signal, 1)
	handleSignals(mountOk, caughtSignal)

	c, err := fuse.Mount(mountPoint, options...)
	if err != nil {
		mountOk <- "" // Neutralize signal handler.

		// Even if fuse.Mount failed, we can still hit the case where the mount point was
		// registered with the kernel. Try to unmount it here as a best-effort operation.
		// (I.e. we can't tell upfront if the mount point was registered so we have to
		// unconditionally try to unmount it and hope it gets cleaned up.)
		//
		// This was observed to happen on Linux: mounting a FUSE file system requires
		// spawning the fusermount program aside from telling the kernel about the mount
		// point. It can happen that a signal arrives at "the wrong time" within the
		// fuse.Mount call above and we get an error here even when the mount point is left
		// behind. This is likely a bug in the fuse.Mount logic.
		fuse.Unmount(mountPoint)

		return newMountError("unable to mount: %v", err)
	}
	defer c.Close()
	mountOk <- mountPoint // Tell signal handler that the mount point requires cleanup.

	err = sandbox.Serve(c, root, reconfigInput, reconfigOutput)
	if err != nil {
		return fmt.Errorf("serve error: %v", err)
	}

	<-c.Ready
	if err := c.MountError; err != nil {
		return fmt.Errorf("mount error: %v", err)
	}

	// If we reach this point, the FUSE serve loop has terminated because the user unmounted the
	// file system or because we received a signal. In both cases we need to exit, but we treat
	// signal receipts as an error just so that the user can tell that the exit was not clean.
	select {
	case signal := <-caughtSignal:
		return fmt.Errorf("caught signal: %v", signal.String())
	default:
	}
	return nil
}

// safeMain is a version of main that does not exit on its own. Instead, it returns an error type
// so that the real main function can format all errors consistently across all commands.
func safeMain(progname string, args []string) error {
	flags := flag.NewFlagSet(progname, flag.ContinueOnError)
	flags.SetOutput(ioutil.Discard)
	flags.Usage = func() {}

	var allow allowFlag
	allow.Set("self")
	flags.Var(&allow, "allow", "specifies who should have access to the file system; must be one of other, root, or self")
	cpuProfile := flags.String("cpu_profile", "", "write a CPU profile to the given file on exit")
	debug := flags.Bool("debug", false, "log details about FUSE requests and responses to stderr")
	help := flags.Bool("help", false, "print the usage information and exit")
	input := flags.String("input", "-", "where to read the configuration data from (- for stdin)")
	listenAddress := flags.String("listen_address", "", "enable HTTP server on the given address and expose pprof data")
	var initialMappings mappingFlag
	flags.Var(&initialMappings, "mapping", "mappings of the form TYPE:MAPPING:TARGET")
	memProfile := flags.String("mem_profile", "", "write a memory profile to the given file on exit")
	output := flags.String("output", "-", "where to write the status of reconfiguration to (- for stdout)")
	version := flags.Bool("version", false, "show version information and exit")
	volumeName := flags.String("volume_name", "sandbox", "name for the sandboxfs volume")

	if err := flags.Parse(args); err != nil {
		if err == flag.ErrHelp {
			// The flags library insists on offering a -h flag even if we explicitly
			// defined --help above. Turn it into an error.
			return newUsageError("flag provided but not defined: -h")
		}
		return newUsageError("%v", err)
	}

	if *help {
		fmt.Fprintf(os.Stdout, "Usage: %s [flags...] mount-point\n\n", progname)
		fmt.Fprintf(os.Stdout, "Available flags:\n")
		flags.SetOutput(os.Stdout)
		flags.PrintDefaults()
		return nil
	}

	if *version {
		fmt.Fprintf(os.Stdout, "sandboxfs %s\n", packageVersion)
		return nil
	}

	settings, err := NewProfileSettings(*cpuProfile, *memProfile, *listenAddress)
	if err != nil {
		return newUsageError("invalid profiling settings: %v", err)
	}

	if *debug {
		fuse.Debug = func(msg interface{}) { fmt.Fprintln(os.Stderr, msg) }
	}

	if flags.NArg() != 1 {
		return newUsageError("invalid number of arguments")
	}
	mountPoint := flags.Arg(0)

	reconfigInput := os.Stdin
	if *input != "-" {
		file, err := os.Open(*input)
		if err != nil {
			return fmt.Errorf("unable to open file %q for reading: %v", *input, err)
		}
		defer file.Close()
		reconfigInput = file
	}
	reconfigOutput := os.Stdout
	if *output != "-" {
		file, err := os.Create(*output)
		if err != nil {
			return fmt.Errorf("unable to open file %q for writing: %v", *output, err)
		}
		defer file.Close()
		reconfigOutput = file
	}

	options := []fuse.MountOption{
		// Rely on in-kernel permission checking based on the node's ownership and mode to
		// avoid having to implement Access -- and we shouldn't implement it because doing
		// so causes a noticeable performance regression with OSXFUSE.
		fuse.DefaultPermissions(),

		fuse.VolumeName(*volumeName),

		// TODO(jmmv): Should be user-customizable.
		fuse.FSName("sandboxfs"),
		fuse.Subtype("sandboxfs"),

		// Do not enable fuse.LocalVolume(): doing so causes macOS to issue additional
		// operations on the file system (e.g. stats on hidden files and tree indexing),
		// which are detrimental to performance. If you feel like you have to add this
		// option, it should probably be user-customizable.
	}
	if allow.Option != nil {
		options = append(options, allow.Option)
	}

	err = serve(settings, mountPoint, options, initialMappings, reconfigInput, reconfigOutput)
	if runtime.GOOS == "linux" && allow.String() == "root" {
		if _, ok := err.(*mountError); ok {
			// "-o allow_root" is broken on Linux because this is not actually a
			// fusermount option: it is a libfuse option and the Go bindings don't
			// implement it as such.  We could implement this on our own by handling
			// allow_root as if it were allow_other with an explicit user check... but
			// it's probably not worth doing.  For now, just tell the user that we know
			// about the breakage.
			//
			// See https://github.com/bazil/fuse/issues/144 for context.
			err = newMountError("%v (-allow=root is known to be broken on Linux)", err)
		}
	}
	return err
}

func main() {
	progname := filepath.Base(os.Args[0])
	args := os.Args[1:]

	if err := safeMain(progname, args); err != nil {
		fmt.Fprintf(os.Stderr, "%v\n", err)

		switch err.(type) {
		case *usageError:
			fmt.Fprintf(os.Stderr, "Type '%s --help' for details\n", progname)
			os.Exit(2)
		default:
			os.Exit(1)
		}
	}
}
