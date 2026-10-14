local n, t0 = 0, os.clock()
for l in io.lines(arg[1]) do n = n + #l end
io.write(string.format("io.lines %d %.0f ms\n", n, (os.clock() - t0) * 1000))
