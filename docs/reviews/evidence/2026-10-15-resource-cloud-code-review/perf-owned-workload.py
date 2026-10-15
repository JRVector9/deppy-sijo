import os, signal, sys, time
# The fixture has its own hard lifetime even if a harness aborts before PID capture.
signal.alarm(15)
if sys.argv[2] == "ignore-hup":
    signal.signal(signal.SIGHUP, signal.SIG_IGN)
with open(sys.argv[1], "x") as output:
    output.write(f"{os.getpid()} {os.getpgrp()} {os.getsid(0)}\n")
while True:
    time.sleep(0.05)
